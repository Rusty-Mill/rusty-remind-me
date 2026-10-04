//! Where and by whom a memory is written (schema v32 provenance).
//!
//! Two questions, answered once per write path and stamped on the row:
//!
//! - **Where**: [`WriteContext`], from the working directory and `git`.
//!   [`current`] resolves the directory (`REMIND_ME_CWD`, else the process's
//!   own), runs `git rev-parse` there and caches the answer, so a write is
//!   not a fork storm.
//! - **Who**: [`Provenance::written_by`] and [`Provenance::capture_method`].
//!
//! `written_by` rule, in one place. `REMIND_ME_WRITTEN_BY` wins when set to
//! a recognised value (`human`, `hook`, `unknown`, `model`, `model:<id>`,
//! `importer:<name>`); hooks set it to `hook`. Otherwise it follows the
//! [`Writer`] the write path declared: the CLI's `add` is `human`, an MCP
//! tool call is `model` (`model:<id>` when `REMIND_ME_MODEL` names one: the
//! MCP client is the *app*, not the model, so its handshake name is not
//! used), an importer is `importer:<source>` and the webhook is
//! `importer:webhook`. An importer is never overridden by the environment:
//! what it imported matters more than what launched it.
//!
//! Every variable here goes through [`crate::daemon::session::var`] like
//! `REMIND_ME_CLIENT`, so one daemon serving many clients stamps each
//! client's own directory and session, not the daemon's.

use crate::daemon::session;
use crate::db::memories::NewMemory;
use crate::models::WriteContext;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Working directory the writing client is in.
pub const CWD_ENV: &str = "REMIND_ME_CWD";
/// The client's session id (a Claude Code hook's `session_id`).
pub const SESSION_ID_ENV: &str = "REMIND_ME_SESSION_ID";
/// Overrides who is credited as the writer.
pub const WRITTEN_BY_ENV: &str = "REMIND_ME_WRITTEN_BY";
/// The model behind MCP tool calls, when the operator knows it.
pub const MODEL_ENV: &str = "REMIND_ME_MODEL";

/// How long one `git` call may run before its field is given up on.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a detected context is reused. Long enough that a burst of
/// writes forks `git` once, short enough that a long-lived server notices a
/// branch switch.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// Longest `REMIND_ME_WRITTEN_BY` or `REMIND_ME_SESSION_ID` accepted.
const MAX_ENV_LEN: usize = 256;

// -- git remote normalisation ------------------------------------------------

/// A remote URL as `host/owner/repo`: no scheme, credentials, port or
/// trailing `.git`.
///
/// One form for `https://user:token@host/o/r.git`, `ssh://git@host:22/o/r`
/// and `git@host:o/r.git`, so the same repository matches however it was
/// cloned, and so a token in `origin` never reaches the store. `None` for
/// an empty URL.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let rest = match url.split_once("://") {
        Some((scheme, rest)) => strip_port(strip_userinfo(rest), scheme == "ssh"),
        None => scp_like(url).unwrap_or_else(|| url.to_string()),
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    (!rest.is_empty()).then(|| lowercase_host(rest))
}

/// `user[:token]@host/path` without the `user[:token]@`.
fn strip_userinfo(rest: &str) -> &str {
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    }
}

/// `host:port/path` without the port, when `drop` (ssh ports say nothing
/// about the repository; an http port may be part of its identity).
fn strip_port(rest: &str, drop: bool) -> String {
    if !drop {
        return rest.to_string();
    }
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let host = authority.split(':').next().unwrap_or(authority);
    format!("{host}{path}")
}

/// `[user@]host:path` (git's scp-like syntax) as `host/path`; `None` when
/// `url` is not that shape, such as a local path.
fn scp_like(url: &str) -> Option<String> {
    if url.starts_with('/') || url.starts_with('.') {
        return None;
    }
    let (authority, path) = url.split_once(':')?;
    if authority.contains('/') {
        return None;
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    Some(format!("{host}/{}", path.trim_start_matches('/')))
}

/// Hosts are case-insensitive, paths are not.
fn lowercase_host(rest: &str) -> String {
    match rest.split_once('/') {
        Some((host, path)) => format!("{}/{path}", host.to_ascii_lowercase()),
        None => rest.to_string(),
    }
}

// -- detection ---------------------------------------------------------------

/// `git <args>` in `cwd`: trimmed stdout, or `None` when git is missing,
/// fails, times out or prints nothing.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

fn basename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
}

/// The context of `cwd`, without a session id. Any git call that fails
/// leaves its field `None`; `project` falls back to the directory's name.
pub fn detect(cwd: &Path) -> WriteContext {
    let toplevel = git(cwd, &["rev-parse", "--show-toplevel"]);
    let cwd_text = cwd.to_string_lossy().into_owned();
    WriteContext {
        project: toplevel
            .as_deref()
            .and_then(basename)
            .or_else(|| basename(&cwd_text)),
        session_id: None,
        git_remote: git(cwd, &["remote", "get-url", "origin"])
            .and_then(|url| normalize_remote(&url)),
        git_branch: git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
        git_sha: git(cwd, &["rev-parse", "HEAD"]),
        cwd: Some(cwd_text),
    }
}

/// A non-empty, bounded, single-line value of `name` as the current session
/// sees it.
fn env_value(name: &str) -> Option<String> {
    let value = session::var(name)?;
    let value = value.trim();
    let ok = !value.is_empty() && value.len() <= MAX_ENV_LEN && !value.contains(['\n', '\r']);
    ok.then(|| value.to_string())
}

/// The directory writes are attributed to: `REMIND_ME_CWD`, else the
/// process's own.
fn resolve_cwd() -> Option<PathBuf> {
    env_value(CWD_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
}

type Cache = Mutex<HashMap<PathBuf, (Instant, WriteContext)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The context writes are stamped with right now: the session's directory
/// detected once per [`CACHE_TTL`], plus `REMIND_ME_SESSION_ID`.
pub fn current() -> WriteContext {
    let session_id = env_value(SESSION_ID_ENV);
    let Some(cwd) = resolve_cwd() else {
        return WriteContext {
            session_id,
            ..WriteContext::default()
        };
    };
    let cached = cache()
        .lock()
        .ok()
        .and_then(|map| map.get(&cwd).cloned())
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, context)| context);
    let mut context = cached.unwrap_or_else(|| {
        let fresh = detect(&cwd);
        if let Ok(mut map) = cache().lock() {
            map.insert(cwd, (Instant::now(), fresh.clone()));
        }
        fresh
    });
    context.session_id = session_id;
    context
}

// -- who wrote it ------------------------------------------------------------

/// Which kind of write path is stamping a row; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Writer {
    /// A person at the CLI or dashboard.
    Human,
    /// An MCP tool call by a model (the default of a server process).
    Model,
    /// A connector, by its `source` (`chat_import`, `obsidian_import`, ...).
    Importer(String),
    /// A hook-driven capture or promotion.
    Hook,
}

/// The process's default [`Writer`] for paths that do not know theirs
/// (`add_memory` serves the CLI, the dashboard and MCP alike). Unset means
/// [`Writer::Model`]: a server process is MCP.
static DEFAULT_WRITER: OnceLock<Writer> = OnceLock::new();

/// Declare this process's default writer. The first call wins; the CLI
/// calls it with [`Writer::Human`] before anything writes.
pub fn set_default_writer(writer: Writer) {
    let _ = DEFAULT_WRITER.set(writer);
}

/// The process default, [`Writer::Model`] unless set.
pub fn default_writer() -> Writer {
    DEFAULT_WRITER.get().cloned().unwrap_or(Writer::Model)
}

/// Fill what a daemon cannot know about its client: the client's directory,
/// and `human` when this process declared itself a person's CLI. Called when
/// a client builds the session it sends, so the daemon stamps the caller's
/// context, not its own.
pub fn fill_client_defaults(vars: &mut std::collections::BTreeMap<String, String>) {
    if !vars.contains_key(CWD_ENV) {
        if let Ok(cwd) = std::env::current_dir() {
            vars.insert(CWD_ENV.to_string(), cwd.to_string_lossy().into_owned());
        }
    }
    if !vars.contains_key(WRITTEN_BY_ENV) && default_writer() == Writer::Human {
        vars.insert(WRITTEN_BY_ENV.to_string(), "human".to_string());
    }
}

/// Whether `value` is a `written_by` this product defines.
pub fn is_valid_written_by(value: &str) -> bool {
    matches!(value, "human" | "hook" | "unknown" | "model")
        || ["model:", "importer:"].iter().any(|prefix| {
            value
                .strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty())
        })
}

/// `written_by` for `writer` given the override and model, both optional.
/// Pure so the rule is testable without touching the environment.
pub fn resolve_written_by(
    writer: &Writer,
    override_value: Option<&str>,
    model: Option<&str>,
) -> String {
    if let Writer::Importer(source) = writer {
        return format!("importer:{source}");
    }
    if let Some(value) = override_value.filter(|v| is_valid_written_by(v)) {
        return value.to_string();
    }
    match (writer, model) {
        (Writer::Human, _) => "human".to_string(),
        (Writer::Hook, _) => "hook".to_string(),
        (_, Some(model)) => format!("model:{model}"),
        _ => "model".to_string(),
    }
}

/// `auto` for anything a machine captured without a person composing it,
/// else `manual`.
pub fn capture_method_for(writer: &Writer) -> &'static str {
    match writer {
        Writer::Importer(_) | Writer::Hook => "auto",
        Writer::Human | Writer::Model => "manual",
    }
}

/// Everything stamped on a newly created memory: who, from which node and
/// client, how, and where. One value per write path (or per import) so no
/// path can stamp some of it and forget the rest (see
/// [`crate::sync::memory_provenance`] for the omission bug this replaced).
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub node_id: String,
    pub client: String,
    pub written_by: String,
    pub capture_method: String,
    pub context: WriteContext,
}

impl Provenance {
    /// Stamp `row`. The context fills only the columns it knows
    /// ([`WriteContext::apply`]); the rest are always written.
    pub fn apply(&self, row: &mut NewMemory) {
        row.node_id = Some(self.node_id.clone());
        row.client.clone_from(&self.client);
        row.written_by.clone_from(&self.written_by);
        row.capture_method.clone_from(&self.capture_method);
        self.context.apply(row);
    }

    /// The same, marked as captured by a machine (`capture_method = auto`):
    /// for the paths that record a conversation or promote one, whoever
    /// triggered them.
    pub fn auto(mut self) -> Self {
        self.capture_method = "auto".to_string();
        self
    }

    /// `row` stamped, for use as a struct-update base:
    /// `NewMemory { category, ..prov.stamp(NewMemory::new(id, text, now)) }`.
    pub fn stamp(&self, mut row: NewMemory) -> NewMemory {
        self.apply(&mut row);
        row
    }
}

/// The [`Provenance`] for a write by `writer`, reading node, client,
/// directory, session and overrides from the current session.
pub fn provenance(writer: &Writer) -> Provenance {
    let (node_id, client) = crate::sync::memory_provenance();
    Provenance {
        node_id,
        client,
        written_by: resolve_written_by(
            writer,
            env_value(WRITTEN_BY_ENV).as_deref(),
            env_value(MODEL_ENV).as_deref(),
        ),
        capture_method: capture_method_for(writer).to_string(),
        context: current(),
    }
}

/// [`provenance`] for the process's [`default_writer`].
pub fn default_provenance() -> Provenance {
    provenance(&default_writer())
}

/// An importer's provenance. The context keeps only the project: an import
/// reads someone else's files, so the branch and sha of wherever the tool
/// ran say nothing about them.
pub fn importer_provenance(source: &str) -> Provenance {
    let mut stamp = provenance(&Writer::Importer(source.to_string()));
    stamp.context = WriteContext {
        project: stamp.context.project.take(),
        ..WriteContext::default()
    };
    stamp
}

// -- search and list filters -------------------------------------------------

/// Narrowing by where and by whom, shared by search and list.
///
/// Exact match, except `project`, which is case-insensitive (directory
/// names are typed by people). Empty strings are treated as unset, so a
/// client sending `""` for "no filter" does not match nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopeFilter {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub written_by: Option<String>,
}

impl ScopeFilter {
    /// Set the field a CLI flag names (`--project`, `--branch`, `--session`,
    /// `--written-by`) to `value`. `false` when `flag` is not one of them.
    pub fn set_from_flag(&mut self, flag: &str, value: &str) -> bool {
        let slot = match flag {
            "--project" => &mut self.project,
            "--branch" => &mut self.branch,
            "--session" => &mut self.session_id,
            "--written-by" => &mut self.written_by,
            _ => return false,
        };
        *slot = Some(value.trim().to_string()).filter(|v| !v.is_empty());
        true
    }

    /// Build from raw inputs, dropping blanks.
    pub fn new(
        project: Option<String>,
        branch: Option<String>,
        session_id: Option<String>,
        written_by: Option<String>,
    ) -> Self {
        let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Self {
            project: clean(project),
            branch: clean(branch),
            session_id: clean(session_id),
            written_by: clean(written_by),
        }
    }

    /// Whether no field narrows anything.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Whether a row with these columns passes every set field.
    pub fn matches(
        &self,
        project: Option<&str>,
        branch: Option<&str>,
        session_id: Option<&str>,
        written_by: &str,
    ) -> bool {
        let want = |wanted: &Option<String>, got: Option<&str>| {
            wanted.as_deref().is_none_or(|w| got == Some(w))
        };
        self.project
            .as_deref()
            .is_none_or(|w| project.is_some_and(|p| p.eq_ignore_ascii_case(w)))
            && want(&self.branch, branch)
            && want(&self.session_id, session_id)
            && self.written_by.as_deref().is_none_or(|w| w == written_by)
    }

    /// [`Self::matches`] over a [`crate::models::Memory`].
    pub fn matches_memory(&self, m: &crate::models::Memory) -> bool {
        self.matches(
            m.project.as_deref(),
            m.git_branch.as_deref(),
            m.session_id.as_deref(),
            &m.written_by,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_forms_normalise_to_one() {
        let same = [
            "https://github.com/acme/widgets.git",
            "https://user:ghp_secret@github.com/acme/widgets",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com:22/acme/widgets.git",
            "https://GitHub.com/acme/widgets/",
        ];
        for url in same {
            assert_eq!(
                normalize_remote(url).as_deref(),
                Some("github.com/acme/widgets"),
                "{url}"
            );
        }
    }

    #[test]
    fn remote_credentials_never_survive() {
        let got = normalize_remote("https://bob:hunter2@example.com/a/b.git").unwrap();
        assert!(!got.contains("hunter2") && !got.contains("bob"));
    }

    #[test]
    fn remote_edge_shapes() {
        assert_eq!(normalize_remote("   "), None);
        assert_eq!(normalize_remote(""), None);
        assert_eq!(
            normalize_remote("/srv/git/widgets.git").as_deref(),
            Some("/srv/git/widgets")
        );
        // An http port can be part of a repository's identity.
        assert_eq!(
            normalize_remote("http://host:8080/o/r.git").as_deref(),
            Some("host:8080/o/r")
        );
    }

    #[test]
    fn written_by_follows_the_documented_rule() {
        let r = |w: Writer, o, m| resolve_written_by(&w, o, m);
        assert_eq!(r(Writer::Human, None, None), "human");
        assert_eq!(r(Writer::Model, None, None), "model");
        assert_eq!(r(Writer::Model, None, Some("opus")), "model:opus");
        assert_eq!(r(Writer::Hook, None, None), "hook");
        // The override wins for people and models, and only when valid.
        assert_eq!(r(Writer::Model, Some("hook"), None), "hook");
        assert_eq!(r(Writer::Human, Some("model:x"), None), "model:x");
        assert_eq!(r(Writer::Model, Some("bogus"), None), "model");
        assert_eq!(r(Writer::Model, Some("importer:"), None), "model");
        // An importer is always itself.
        assert_eq!(
            r(Writer::Importer("webhook".into()), Some("hook"), None),
            "importer:webhook"
        );
    }

    #[test]
    fn capture_method_is_auto_for_machines() {
        assert_eq!(capture_method_for(&Writer::Hook), "auto");
        assert_eq!(capture_method_for(&Writer::Importer("x".into())), "auto");
        assert_eq!(capture_method_for(&Writer::Human), "manual");
        assert_eq!(capture_method_for(&Writer::Model), "manual");
    }

    #[test]
    fn scope_filter_matches_exactly_but_project_ignores_case() {
        let f = ScopeFilter::new(Some("Widgets".into()), Some("main".into()), None, None);
        assert!(f.matches(Some("widgets"), Some("main"), None, "human"));
        assert!(!f.matches(Some("widgets"), Some("Main"), None, "human"));
        assert!(!f.matches(None, Some("main"), None, "human"));
        let by = ScopeFilter::new(None, None, Some("s1".into()), Some("hook".into()));
        assert!(by.matches(None, None, Some("s1"), "hook"));
        assert!(!by.matches(None, None, Some("s1"), "human"));
        assert!(!by.matches(None, None, None, "hook"));
    }

    #[test]
    fn scope_flags_set_their_fields() {
        let mut f = ScopeFilter::default();
        assert!(f.set_from_flag("--project", " rusty "));
        assert!(f.set_from_flag("--branch", "main"));
        assert!(f.set_from_flag("--session", "s9"));
        assert!(f.set_from_flag("--written-by", "hook"));
        assert!(!f.set_from_flag("--category", "x"));
        assert_eq!(
            f,
            ScopeFilter::new(
                Some("rusty".into()),
                Some("main".into()),
                Some("s9".into()),
                Some("hook".into())
            )
        );
        assert!(f.set_from_flag("--project", " "));
        assert_eq!(f.project, None);
    }

    #[test]
    fn client_defaults_add_the_directory_but_never_overwrite() {
        let mut vars = std::collections::BTreeMap::new();
        fill_client_defaults(&mut vars);
        assert!(vars.contains_key(CWD_ENV));
        let mut set = std::collections::BTreeMap::from([(CWD_ENV.to_string(), "/x".to_string())]);
        fill_client_defaults(&mut set);
        assert_eq!(set[CWD_ENV], "/x");
    }

    #[test]
    fn scope_filter_treats_blanks_as_unset() {
        let f = ScopeFilter::new(Some(" ".into()), Some("".into()), None, None);
        assert!(f.is_empty());
        assert!(f.matches(None, None, None, "unknown"));
    }

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "git {args:?} failed");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rmm-ctx-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detect_reads_a_git_repository() {
        if !git_available() {
            return;
        }
        let root = temp_dir("repo");
        let repo = root.join("widgets");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        run(&repo, &["init", "-q", "-b", "trunk"]);
        run(&repo, &["config", "user.email", "t@example.com"]);
        run(&repo, &["config", "user.name", "T"]);
        run(&repo, &["config", "commit.gpgsign", "false"]);
        run(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://u:tok@github.com/acme/widgets.git",
            ],
        );
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-q", "-m", "init"]);

        let ctx = detect(&repo.join("sub"));
        assert_eq!(ctx.project.as_deref(), Some("widgets"));
        assert_eq!(ctx.git_branch.as_deref(), Some("trunk"));
        assert_eq!(ctx.git_remote.as_deref(), Some("github.com/acme/widgets"));
        assert_eq!(ctx.git_sha.as_ref().map(String::len), Some(40));
        assert_eq!(
            ctx.cwd,
            Some(repo.join("sub").to_string_lossy().into_owned())
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn detect_outside_git_falls_back_to_the_directory_name() {
        if !git_available() {
            return;
        }
        let dir = temp_dir("plain");
        let ctx = detect(&dir);
        // A temp dir could sit inside some repo on an odd machine; the
        // fallback only applies when it does not.
        if ctx.git_sha.is_none() {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(ctx.project, Some(name));
            assert_eq!(ctx.git_branch, None);
            assert_eq!(ctx.git_remote, None);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn detect_a_missing_directory_leaves_git_fields_empty() {
        let ctx = detect(Path::new("/definitely/not/a/dir"));
        assert_eq!(ctx.git_sha, None);
        assert_eq!(ctx.project.as_deref(), Some("dir"));
    }

    #[test]
    fn importer_provenance_keeps_only_the_project() {
        let p = importer_provenance("chat_import");
        assert_eq!(p.written_by, "importer:chat_import");
        assert_eq!(p.capture_method, "auto");
        assert_eq!(p.context.git_sha, None);
        assert_eq!(p.context.cwd, None);
        assert_eq!(p.context.session_id, None);
    }

    #[test]
    fn stamp_writes_every_provenance_column() {
        let p = Provenance {
            node_id: "n1".into(),
            client: "cli".into(),
            written_by: "hook".into(),
            capture_method: "auto".into(),
            context: WriteContext {
                project: Some("p".into()),
                git_branch: Some("b".into()),
                ..WriteContext::default()
            },
        };
        let row = p.stamp(NewMemory::new("mem_1", "text", "2026-01-01T00:00:00Z"));
        assert_eq!(row.node_id.as_deref(), Some("n1"));
        assert_eq!(row.client, "cli");
        assert_eq!(row.written_by, "hook");
        assert_eq!(row.capture_method, "auto");
        assert_eq!(row.project.as_deref(), Some("p"));
        assert_eq!(row.git_branch.as_deref(), Some("b"));
        assert_eq!(row.cwd, None);
    }
}
