//! Child processes that live and die with this server.
//!
//! Two things want to be running whenever the server is, and neither is worth
//! a separate service manager: the SSH tunnel that fronts the hub, and
//! optionally the dashboard UI. [`ensure_sidecars`] is idempotent and is meant
//! to be called from the sync loop, so a sidecar lost to another server's exit
//! comes back within one interval.
//!
//! # Teardown: what is and is not guaranteed
//!
//! This is the part worth reading before trusting the module.
//!
//! **Graceful exit is covered on every platform.** [`Sidecars`] kills its
//! children on drop, and unwinding runs `Drop`, so a normal return or a panic
//! both tear the children down.
//!
//! **Abnormal exit is covered on Windows only** — and that is the reference's
//! shape, not a shortfall from it. Every child is assigned to a Windows **Job
//! object** created with `KILL_ON_JOB_CLOSE` ([`job`]), so when the parent's
//! handle closes for *any* reason — `TerminateProcess`, a hard crash, a power
//! cut — the OS reaps the whole job. No exit hook is involved, which is
//! precisely what makes it robust. The reference's `_job()` returns `None`
//! immediately when `sys.platform != "win32"`, so on Linux and macOS neither
//! it nor this module has any such guarantee: a `SIGKILL`ed server orphans its
//! sidecars.
//!
//! So relative to the reference this module is:
//!
//! | Platform | Reference | Here |
//! | --- | --- | --- |
//! | Windows, graceful | children killed | children killed |
//! | Windows, abnormal | children killed (Job object) | children killed (Job object) |
//! | Unix, graceful | children killed | children killed |
//! | Unix, abnormal | children orphaned | children orphaned |
//!
//! All four cells match. `docs/adr/0013` records both halves of that history:
//! the original decision to ship without the Job object, and the amendment
//! that closed it by taking this workspace's first — and still only — FFI
//! dependency, target-gated to Windows.
//!
//! The remaining Unix gap has a mild practical symptom: an SSH tunnel
//! surviving a `SIGKILL`ed server keeps holding its local port, and the next
//! server's `ensure_sidecars` sees the port answering and declines to start
//! its own — which is *usually* fine, since the surviving tunnel still works,
//! and is why this is a gap rather than an outage.
//!
//! One caveat worth stating plainly: the Windows path is **type-checked but
//! not runtime-tested**. CI runs on Ubuntu only, so `mod job` is verified by
//! `cargo check --target x86_64-pc-windows-gnu` and by inspection against the
//! reference's `ctypes` calls, and by nothing else.
//!
//! # Why the port, not the process, decides
//!
//! A sidecar is wanted when its port is not answering — not when this process
//! happens to have no handle to one. Several servers run against the same
//! database (one per connected client is the normal deployment), so a tunnel
//! started by a sibling is a perfectly good tunnel. Checking the port is what
//! makes that work; checking `self.procs` would start a second one.

use std::collections::HashMap;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Full command line for the hub tunnel. Unset or empty = no tunnel.
pub const TUNNEL_ENV: &str = "REMIND_ME_TUNNEL";

/// `1`/`true`/`yes` to also keep the dashboard UI alive.
pub const SIDECAR_UI_ENV: &str = "REMIND_ME_SIDECAR_UI";

/// Port the dashboard sidecar is kept alive on.
pub const UI_PORT_ENV: &str = "REMIND_ME_MCP_UI_PORT";

/// Matches the reference's `config.UI_PORT` default.
pub const DEFAULT_UI_PORT: u16 = 5199;

/// How long a port probe waits before calling the port closed.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to wait for a freshly-spawned tunnel to start answering.
///
/// The reference polls 20 times at 0.5s. The wait exists so the first sync
/// after startup succeeds rather than failing against a tunnel that is still
/// negotiating.
const TUNNEL_WAIT_ATTEMPTS: usize = 20;
const TUNNEL_WAIT_INTERVAL: Duration = Duration::from_millis(500);

/// Is a port answering?
///
/// Any connect failure means "not answering". Distinguishing refused from
/// timed out from unresolvable would not change the decision — all three mean
/// the sidecar is wanted.
fn port_open(host: &str, port: u16) -> bool {
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
}

/// Split a command line into program and arguments.
///
/// Backslash handling is platform-dependent, and has to be: on Windows it is
/// a path separator, so `"C:\Program Files\ssh.exe"` must survive intact,
/// while on Unix it is the escape character. The reference makes exactly this
/// split — `shlex.split(TUNNEL_CMD, posix=sys.platform != "win32")` — and
/// getting it wrong mangles every Windows tunnel command into an unrunnable
/// path.
///
/// Deliberately not a full shell parser: the command is spawned directly,
/// never through a shell, so glob and variable expansion are not merely
/// unimplemented but unwanted.
fn split_command(raw: &str) -> Vec<String> {
    split_command_mode(raw, !cfg!(windows))
}

/// The platform-independent core, so both modes are testable everywhere.
///
/// `posix` selects whether `\` escapes the next character or is a literal.
fn split_command_mode(raw: &str, posix: bool) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    let mut quote: Option<char> = None;
    let mut has_token = false;

    while let Some(c) = chars.next() {
        match c {
            '\\' if posix && quote != Some('\'') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    has_token = true;
                }
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                has_token = true;
            }
            c if Some(c) == quote => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if has_token {
                    parts.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        parts.push(current);
    }
    parts
}

/// Resolve the dashboard port the UI sidecar should hold.
pub fn ui_port() -> u16 {
    std::env::var(UI_PORT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(DEFAULT_UI_PORT)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// What one sidecar wants: a name, a command, and the port that proves it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarSpec {
    pub name: String,
    pub command: Vec<String>,
    pub host: String,
    pub port: u16,
    /// Environment overrides applied on top of this process's environment.
    /// A `None` value *removes* the variable, which is how the UI sidecar
    /// avoids inheriting `REMIND_ME_HUB_URL`.
    pub env: Vec<(String, Option<String>)>,
    /// Whether to wait for the port after spawning.
    pub wait_for_port: bool,
}

/// Supervises the configured sidecars.
///
/// Children are killed on drop. See the module docs for exactly which exit
/// paths that does and does not cover.
#[derive(Debug, Default)]
pub struct Sidecars {
    procs: HashMap<String, Child>,
}

impl Sidecars {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start any configured sidecar whose port is not answering.
    ///
    /// Idempotent, and safe to call on a timer — that is the intended use.
    pub fn ensure(&mut self) {
        self.ensure_specs(&configured_sidecars());
    }

    /// [`ensure`][Self::ensure] against an explicit spec list.
    ///
    /// Separated so the supervision behaviour — spawn, skip, reap, respawn —
    /// can be driven from a test with a harmless command, instead of only
    /// through environment variables and a real SSH tunnel.
    pub fn ensure_specs(&mut self, specs: &[SidecarSpec]) {
        for spec in specs {
            if port_open(&spec.host, spec.port) {
                continue;
            }
            if let Err(e) = self.spawn(spec) {
                eprintln!("sidecars: could not start {}: {}", spec.name, e);
                continue;
            }
            if spec.wait_for_port {
                self.wait_for_port(spec);
            }
        }
    }

    /// Spawn one sidecar, reaping any dead predecessor first.
    fn spawn(&mut self, spec: &SidecarSpec) -> io::Result<()> {
        // Reap before respawning. Without this, a persistently-failing sidecar
        // (bad key, unreachable host) leaks one zombie per tick, forever, for
        // as long as the misconfiguration lasts -- the reference hit exactly
        // this as its issue #139.
        if let Some(existing) = self.procs.get_mut(&spec.name) {
            match existing.try_wait() {
                Ok(None) => return Ok(()), // still running; nothing to do
                Ok(Some(status)) => {
                    eprintln!("sidecars: {} exited ({}), restarting", spec.name, status);
                }
                Err(e) => {
                    eprintln!("sidecars: cannot poll {}: {}", spec.name, e);
                }
            }
            self.procs.remove(&spec.name);
        }

        let Some((program, args)) = spec.command.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty sidecar command",
            ));
        };

        let mut cmd = Command::new(program);
        cmd.args(args)
            // Detached from this process's stdio: a sidecar writing to the
            // MCP server's stdout would corrupt the JSON-RPC stream, which is
            // the one truly unrecoverable failure available here.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in &spec.env {
            match value {
                Some(v) => cmd.env(key, v),
                None => cmd.env_remove(key),
            };
        }

        let child = cmd.spawn()?;
        // Immediately after spawn, before anything can return early: an
        // unassigned child is exactly the orphan this exists to prevent.
        job::assign(&child, &spec.name);
        eprintln!("sidecars: {} started (pid {})", spec.name, child.id());
        self.procs.insert(spec.name.clone(), child);
        Ok(())
    }

    /// Give a freshly-spawned sidecar a moment to start answering.
    ///
    /// Gives up early if the child exits — a tunnel that died on a bad key is
    /// never going to open the port, and waiting the full budget for it just
    /// delays the log line that says so.
    fn wait_for_port(&mut self, spec: &SidecarSpec) {
        for _ in 0..TUNNEL_WAIT_ATTEMPTS {
            if port_open(&spec.host, spec.port) {
                return;
            }
            if let Some(child) = self.procs.get_mut(&spec.name) {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!("sidecars: {} exited early ({})", spec.name, status);
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
            std::thread::sleep(TUNNEL_WAIT_INTERVAL);
        }
        eprintln!(
            "sidecars: {} did not open {}:{} within {:?}",
            spec.name,
            spec.host,
            spec.port,
            TUNNEL_WAIT_INTERVAL * TUNNEL_WAIT_ATTEMPTS as u32
        );
    }

    /// PIDs of the sidecars this instance currently holds a handle to.
    ///
    /// An operator wanting to look one up, and the only non-racy way for a
    /// test to identify the child it just started — matching by command line
    /// through the process table can find a leaked one from an earlier run.
    pub fn pids(&mut self) -> Vec<(String, u32)> {
        let mut out: Vec<(String, u32)> = self
            .procs
            .iter_mut()
            .filter_map(|(name, child)| match child.try_wait() {
                Ok(None) => Some((name.clone(), child.id())),
                _ => None,
            })
            .collect();
        out.sort();
        out
    }

    /// Names of the sidecars this instance currently holds a handle to.
    pub fn running(&mut self) -> Vec<String> {
        let mut names: Vec<String> = self
            .procs
            .iter_mut()
            .filter_map(|(name, child)| match child.try_wait() {
                Ok(None) => Some(name.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names
    }

    /// Kill and reap every sidecar this instance started.
    pub fn shutdown(&mut self) {
        for (name, mut child) in self.procs.drain() {
            // A child that already exited makes `kill` fail; that is the
            // success case, not an error worth reporting.
            let _ = child.kill();
            match child.wait() {
                Ok(_) => eprintln!("sidecars: {} stopped", name),
                Err(e) => eprintln!("sidecars: {} could not be reaped: {}", name, e),
            }
        }
    }
}

impl Drop for Sidecars {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Which sidecars the environment asks for, in the order they are started.
///
/// Public so a caller can see what would be started without starting it, and
/// so the configuration logic is testable without spawning processes.
pub fn configured_sidecars() -> Vec<SidecarSpec> {
    let mut specs = Vec::new();

    let tunnel = std::env::var(TUNNEL_ENV).unwrap_or_default();
    // Read through the public env const rather than `sync`'s private
    // `configured_hub_url`: widening that module's API for one read here
    // would be the wrong direction of coupling.
    let hub_url = std::env::var(crate::sync::HUB_URL_ENV).unwrap_or_default();
    if !tunnel.trim().is_empty() && !hub_url.trim().is_empty() {
        if let Some((host, port)) = hub_host_port(&hub_url) {
            let command = split_command(&tunnel);
            if !command.is_empty() {
                specs.push(SidecarSpec {
                    name: "tunnel".to_string(),
                    command,
                    host,
                    port,
                    env: Vec::new(),
                    wait_for_port: true,
                });
            }
        }
    }

    if env_flag(SIDECAR_UI_ENV) {
        if let Ok(exe) = std::env::current_exe() {
            let port = ui_port();
            specs.push(SidecarSpec {
                name: "ui".to_string(),
                command: vec![
                    exe.display().to_string(),
                    "api".to_string(),
                    port.to_string(),
                ],
                host: "127.0.0.1".to_string(),
                port,
                env: vec![
                    // Without this the dashboard would try to keep its own UI
                    // sidecar alive -- an unbounded fork bomb of dashboards.
                    (SIDECAR_UI_ENV.to_string(), Some("0".to_string())),
                    // The dashboard is a local reader; it has no business
                    // opening its own hub connection.
                    (crate::sync::HUB_URL_ENV.to_string(), None),
                ],
                wait_for_port: false,
            });
        }
    }

    specs
}

/// Pull host and port out of a hub URL, defaulting the port by scheme.
fn hub_host_port(raw: &str) -> Option<(String, u16)> {
    let rest = raw
        .split_once("://")
        .map(|(scheme, rest)| (scheme.to_ascii_lowercase(), rest));
    let (scheme, rest) = match rest {
        Some((scheme, rest)) => (scheme, rest),
        // No scheme: treat the whole thing as a host, same as an http URL.
        None => ("http".to_string(), raw),
    };
    // Strip any path, query or fragment; strip credentials if present.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let default_port = if scheme == "https" { 443 } else { 80 };

    // An IPv6 literal is bracketed, so the last colon only separates a port
    // when it comes after the closing bracket.
    if let Some(close) = authority.rfind(']') {
        let host = authority[..=close].to_string();
        let port = authority[close + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            Some((host.to_string(), port.parse().unwrap_or(default_port)))
        }
        _ => Some((authority.to_string(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_command_splits_on_whitespace() {
        assert_eq!(
            split_command("ssh -N -L 8080:localhost:8080 hub"),
            vec!["ssh", "-N", "-L", "8080:localhost:8080", "hub"]
        );
    }

    #[test]
    fn quoted_segments_survive_their_spaces() {
        assert_eq!(
            split_command("ssh -o 'StrictHostKeyChecking no' host"),
            vec!["ssh", "-o", "StrictHostKeyChecking no", "host"]
        );
    }

    /// The whole reason backslash handling is platform-dependent: a Windows
    /// tunnel command names an executable by path, and treating `\` as an
    /// escape turns `C:\Program Files\ssh.exe` into `C:Program Filesssh.exe`
    /// — a path that does not exist, so the sidecar never starts.
    #[test]
    fn a_windows_path_keeps_its_backslashes() {
        assert_eq!(
            split_command_mode(r#""C:\Program Files\ssh.exe" -N host"#, false),
            vec![r"C:\Program Files\ssh.exe", "-N", "host"]
        );
    }

    /// And the Unix side still escapes, which is how a literal space reaches
    /// an argument without quotes around it.
    #[test]
    fn posix_mode_still_treats_backslash_as_an_escape() {
        assert_eq!(
            split_command_mode(r"ssh -o Foo\ Bar host", true),
            vec!["ssh", "-o", "Foo Bar", "host"]
        );
    }

    /// Single quotes suppress escaping in posix mode, matching `shlex`.
    #[test]
    fn a_single_quoted_backslash_is_literal_even_in_posix_mode() {
        assert_eq!(
            split_command_mode(r"ssh -i 'a\b' host", true),
            vec!["ssh", "-i", r"a\b", "host"]
        );
    }

    #[test]
    fn an_empty_command_yields_no_parts() {
        assert!(split_command("").is_empty());
        assert!(split_command("   ").is_empty());
    }

    /// An empty quoted argument is a real argument, not whitespace.
    #[test]
    fn an_empty_quoted_argument_is_preserved() {
        assert_eq!(split_command(r#"ssh -o """#), vec!["ssh", "-o", ""]);
    }

    #[test]
    fn hub_ports_default_by_scheme() {
        assert_eq!(
            hub_host_port("https://hub.example"),
            Some(("hub.example".to_string(), 443))
        );
        assert_eq!(
            hub_host_port("http://hub.example"),
            Some(("hub.example".to_string(), 80))
        );
    }

    #[test]
    fn an_explicit_hub_port_wins() {
        assert_eq!(
            hub_host_port("http://127.0.0.1:8080/sync"),
            Some(("127.0.0.1".to_string(), 8080))
        );
        assert_eq!(
            hub_host_port("https://hub.example:9443/"),
            Some(("hub.example".to_string(), 9443))
        );
    }

    #[test]
    fn an_ipv6_literal_is_not_split_on_its_own_colons() {
        assert_eq!(
            hub_host_port("http://[::1]:8080/sync"),
            Some(("[::1]".to_string(), 8080))
        );
        assert_eq!(
            hub_host_port("https://[2001:db8::1]"),
            Some(("[2001:db8::1]".to_string(), 443))
        );
    }

    #[test]
    fn credentials_and_paths_are_stripped() {
        assert_eq!(
            hub_host_port("http://user:pw@hub.example:8080/a/b?c=d#e"),
            Some(("hub.example".to_string(), 8080))
        );
    }

    #[test]
    fn a_hub_url_with_no_scheme_is_still_usable() {
        assert_eq!(
            hub_host_port("hub.example:8080"),
            Some(("hub.example".to_string(), 8080))
        );
    }

    #[test]
    fn an_empty_hub_url_yields_nothing() {
        assert_eq!(hub_host_port(""), None);
        assert_eq!(hub_host_port("http://"), None);
    }

    /// A closed port must read as closed — this is the signal the whole module
    /// keys on, so a probe that reported "open" for a dead port would start
    /// nothing, forever.
    #[test]
    fn a_closed_port_reads_as_closed() {
        // Bind and immediately drop, so the port is real but unbound.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!port_open("127.0.0.1", port));
    }

    #[test]
    fn a_listening_port_reads_as_open() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_open("127.0.0.1", port));
    }

    #[test]
    fn an_unresolvable_host_reads_as_closed_rather_than_panicking() {
        assert!(!port_open("no-such-host.invalid", 80));
    }
}

/// Windows Job-object teardown: the OS kills the sidecars when this process
/// dies, however it dies.
///
/// `Drop` covers a graceful exit and an unwinding panic, but not
/// `TerminateProcess`, a hard crash, or a power cut. A Job object created with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` covers all of those: the kernel
/// terminates every assigned process when the last handle to the job closes,
/// which happens automatically when the process object is destroyed. No exit
/// hook is involved, which is precisely what makes it robust.
///
/// This is the cell `docs/adr/0013` recorded as deliberately missing. It is
/// closed now; the ADR carries the amendment.
///
/// # Unix has no equivalent, and neither does the reference
///
/// The reference's `_job()` returns `None` immediately when
/// `sys.platform != "win32"`. `prctl(PR_SET_PDEATHSIG)` is the nearest Linux
/// analogue, is Linux-only rather than Unix-wide, and would need a second FFI
/// dependency to reach — so the Unix rows stay as they were, matching the
/// reference exactly.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A `HANDLE` is a raw pointer, so it is neither `Send` nor `Sync` by
    /// default. Wrapping it is sound here because a job handle is an opaque
    /// kernel identifier rather than something dereferenced: it is only ever
    /// passed back to Win32 calls, which are themselves thread-safe, and it is
    /// created exactly once and never mutated afterwards.
    struct JobHandle(HANDLE);
    // SAFETY: see the type's own docs -- the handle is never dereferenced,
    // only handed back to thread-safe Win32 APIs.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

    /// Create the process-wide job, once.
    ///
    /// Returns `None` when the job could not be created or configured, which
    /// is a *degradation*, never a failure: sidecars still start and are still
    /// killed on a graceful exit. Only the abnormal-exit guarantee is lost, so
    /// this warns rather than propagating.
    fn job() -> Option<HANDLE> {
        JOB.get_or_init(|| {
            // SAFETY: both arguments are optional per the Win32 contract; a
            // null security descriptor and null name request an unnamed job
            // with default security, which is what is wanted.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                // SAFETY: reads a thread-local error code set by the call
                // immediately above; no pointers involved.
                let err = unsafe { GetLastError() };
                eprintln!(
                    "sidecars: CreateJobObjectW failed (GetLastError={err}) -- sidecars will \
                     not be killed automatically if this process exits abnormally"
                );
                return None;
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                // SAFETY: the struct is plain old data with no padding
                // invariants and no pointers; Win32 expects a zeroed struct
                // with only the fields it is told to read populated.
                unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: `info` outlives the call, and the length matches the
            // struct the class argument selects.
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                // SAFETY: reads a thread-local error code set by the call
                // immediately above; no pointers involved.
                let err = unsafe { GetLastError() };
                eprintln!(
                    "sidecars: SetInformationJobObject failed (GetLastError={err}) -- \
                     KILL_ON_JOB_CLOSE not set, so sidecars may outlive this process"
                );
                // Deliberate divergence: the reference warns here but keeps
                // the handle and still assigns children to it. A job without
                // KILL_ON_JOB_CLOSE grants nothing, so that buys no teardown
                // guarantee -- it just leaks a kernel handle and makes every
                // later `assign` do pointless work and emit pointless
                // warnings. Sidecar behaviour is identical either way; this
                // is tidier. Recorded in `docs/parity-loop-decisions.md`.
                //
                // SAFETY: `handle` is a valid handle this function created and
                // is not stored anywhere after this point.
                unsafe { CloseHandle(handle) };
                return None;
            }
            Some(JobHandle(handle))
        })
        .as_ref()
        .map(|j| j.0)
    }

    /// Put a freshly-spawned child under the job.
    ///
    /// A failure is reported and tolerated for the same reason the job's own
    /// creation failure is: the sidecar is running and useful either way. The
    /// commonest cause is this process already living inside a job without
    /// `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, which the reference hit as its issue
    /// #138 and which no amount of care here can fix from the inside.
    pub(super) fn assign(child: &Child, name: &str) {
        let Some(job) = job() else {
            return;
        };
        // SAFETY: the handle belongs to a live `Child` borrowed for this call,
        // so it cannot have been closed.
        let ok = unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            // SAFETY: reads a thread-local error code set by the call
            // immediately above; no pointers involved.
            let err = unsafe { GetLastError() };
            eprintln!(
                "sidecars: AssignProcessToJobObject failed for {name} (GetLastError={err}) \
                 -- it will not be auto-killed if this process exits abnormally"
            );
        }
    }
}

/// No-op on every non-Windows target, matching the reference's own `_job()`.
#[cfg(not(windows))]
mod job {
    pub(super) fn assign(_child: &std::process::Child, _name: &str) {}
}
