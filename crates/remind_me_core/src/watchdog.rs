//! Visibility into a tool call that is stuck, not merely slow.
//!
//! The gap this closes (reference issue #128): when a call hangs, the only
//! symptom visible from outside the process is the client reporting a timeout.
//! Nothing in the server says *which* call, or how long it has been going.
//! Diagnosing a real incident on the reference side — a correlated subquery
//! pegging a core for minutes — meant attaching `py-spy` to a live process.
//!
//! # Two levels of detail, and how to get the better one
//!
//! The reference arms `faulthandler.dump_traceback_later`, which dumps *every*
//! thread's stack, including one blocked in synchronous CPU-bound code. Rust
//! has no stdlib equivalent, so this module has two modes:
//!
//! - **Always: identity and duration.** *Which* call has been running, and for
//!   how long. This needs nothing beyond `std` and works on every platform.
//! - **With the `stack-dumps` feature, on Linux: every thread's stack**, the
//!   reference's actual guarantee. See [`install_stack_dump_hook`], which the
//!   binary must call — without it, no dump is ever attempted.
//!
//! The feature is off by default, and deliberately so: it needs a *system*
//! library (`libunwind-ptrace`) and permission to `ptrace`, which is a heavier
//! ask than every other optional feature in this crate. `docs/adr/0014`
//! records that trade in full. Feature-off the watchdog is exactly what it was
//! — a strictly weaker signal than the reference's, said plainly here rather
//! than left for someone to discover when the dump they expected is a log
//! line.
//!
//! # Why out-of-process, and why that is not paranoia
//!
//! The dump works by spawning a short-lived child that `ptrace`s this process.
//! The obvious alternative — a signal handler that unwinds the stuck thread in
//! place — is what a profiler like `pprof` does, and it is not safe here.
//! Capturing a backtrace is not async-signal-safe: it allocates and takes
//! loader locks, so a thread interrupted while already holding one deadlocks,
//! permanently, in the exact situation the diagnostic exists to explain. The
//! reference states the rule this module inherits — *this must never be the
//! reason a tool call fails* — and an in-process unwinder cannot honour it.
//! Being ptraced from outside costs the target nothing but a stop.
//!
//! # Reference-counted, because calls overlap
//!
//! Arming is not per-call: several tool calls can be in flight at once, and a
//! watchdog tied to one of them would disarm while the others are still
//! running. The reference counts in-flight calls and only cancels on the
//! transition back to zero. Here that counting is [`CallGuard`]'s job —
//! dropping the guard is what disarms, so an early return or a panic cannot
//! leak a permanently-armed watchdog the way an unbalanced manual `disarm()`
//! can.
//!
//! # Off by default is not the same as disabled by default
//!
//! The threshold is read from [`SLOW_CALL_SECONDS_ENV`] and defaults to 30
//! seconds, matching the reference. `0` disables the watchdog entirely, also
//! matching. A malformed value falls back to the default rather than
//! disabling: a typo in a diagnostic's tuning knob should not silently turn the
//! diagnostic off.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Environment variable holding the threshold, in seconds.
pub const SLOW_CALL_SECONDS_ENV: &str = "REMIND_ME_SLOW_CALL_SECONDS";

/// Threshold used when [`SLOW_CALL_SECONDS_ENV`] is unset or malformed.
pub const DEFAULT_SLOW_CALL_SECONDS: f64 = 30.0;

/// A call that has been running longer than the threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckCall {
    /// The tool name, e.g. `remind_me_search`.
    pub tool: String,
    /// How long it had been running when the watchdog noticed.
    pub elapsed: Duration,
    /// Every thread's stack at the moment the watchdog noticed, when
    /// [`stack_dumps_available`] is true and the capture succeeded.
    ///
    /// `None` covers three different situations on purpose — feature off,
    /// hook not installed, capture failed — because none of them changes what
    /// a caller does: report what is known and carry on. The distinction is
    /// logged at the point it is discovered, where the reason is still in
    /// hand, rather than encoded in a type nobody matches on.
    pub stacks: Option<String>,
}

/// Where a stuck-call report goes.
///
/// Injectable so the firing behaviour is testable without capturing stderr or
/// waiting on a real 30-second threshold. Production uses [`stderr_sink`].
pub type Sink = Arc<dyn Fn(&StuckCall) + Send + Sync>;

/// The default sink: one line per stuck call, matching this crate's
/// `subsystem: message` convention, followed by the thread stacks when there
/// are any.
///
/// Stacks go to stderr rather than a file, matching where the reference points
/// `faulthandler` — the operator reading "the call timed out" is already
/// looking at this stream.
pub fn stderr_sink() -> Sink {
    Arc::new(|stuck: &StuckCall| {
        eprintln!(
            "watchdog: {} has been running for {:.1}s",
            stuck.tool,
            stuck.elapsed.as_secs_f64()
        );
        if let Some(stacks) = &stuck.stacks {
            eprintln!("{}", stacks);
        }
    })
}

/// Resolve the configured threshold. `None` means the watchdog is disabled.
///
/// Read on construction rather than per-call: the reference reads its own
/// `SLOW_CALL_SECONDS` once at import, and a threshold that changes underneath
/// an in-flight call would make the report unreproducible.
pub fn configured_threshold() -> Option<Duration> {
    let seconds = match std::env::var(SLOW_CALL_SECONDS_ENV) {
        Ok(raw) => raw
            .trim()
            .parse::<f64>()
            .unwrap_or(DEFAULT_SLOW_CALL_SECONDS),
        Err(_) => DEFAULT_SLOW_CALL_SECONDS,
    };
    if seconds > 0.0 && seconds.is_finite() {
        Some(Duration::from_secs_f64(seconds))
    } else {
        // 0 (or a negative/NaN value) is the reference's "off" switch.
        None
    }
}

/// Thread-stack capture. Real only with `stack-dumps` on Linux; a no-op
/// everywhere else, so the call sites need no `cfg` of their own.
mod stacks {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set on the child process so it knows to become a tracer instead of
    /// running the program again.
    ///
    /// An env var rather than an argv flag deliberately: argv is the binary's
    /// public interface, and a stray `--rstack-child` in `--help` output — or
    /// worse, colliding with a real subcommand — is a cost this diagnostic has
    /// no business imposing.
    pub const CHILD_MARKER_ENV: &str = "REMIND_ME_WATCHDOG_STACK_CHILD";

    /// Whether the binary called [`super::install_stack_dump_hook`].
    ///
    /// This is a safety interlock, not bookkeeping. Capturing works by
    /// re-executing this binary; if the hook is absent, that child would run
    /// the *program* — starting a second MCP server behind the operator's
    /// back. So nothing is ever spawned unless the hook has positively
    /// announced itself.
    static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

    pub fn mark_hook_installed() {
        HOOK_INSTALLED.store(true, Ordering::SeqCst);
    }

    pub fn hook_installed() -> bool {
        HOOK_INSTALLED.load(Ordering::SeqCst)
    }

    /// Is this build able to dump stacks at all, ignoring the hook?
    pub const fn compiled_in() -> bool {
        cfg!(all(target_os = "linux", feature = "stack-dumps"))
    }

    #[cfg(all(target_os = "linux", feature = "stack-dumps"))]
    pub fn capture() -> Result<String, String> {
        use std::fmt::Write as _;

        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.env(CHILD_MARKER_ENV, "1");
        // The child's job is to report through the pipe rstack-self sets up.
        // Anything it writes to stdout would interleave with the MCP protocol
        // on a stdio server, so it is silenced rather than inherited.
        cmd.stdout(std::process::Stdio::null());

        let trace = rstack_self::trace(&mut cmd).map_err(|e| format!("trace: {e}"))?;

        let mut out = String::from("watchdog: thread stacks follow\n");
        for thread in trace.threads() {
            let _ = writeln!(out, "  thread {} ({:?})", thread.id(), thread.name());
            for frame in thread.frames() {
                // A frame can resolve to several symbols when it is inlined,
                // and to none at all when the binary is stripped -- both are
                // normal, and neither is worth failing the whole dump over.
                let mut named = false;
                for symbol in frame.symbols() {
                    named = true;
                    let _ = write!(out, "    {}", symbol.name().unwrap_or("<unknown>"));
                    match (symbol.file(), symbol.line()) {
                        (Some(file), Some(line)) => {
                            let _ = writeln!(out, " ({}:{})", file.display(), line);
                        }
                        (Some(file), None) => {
                            let _ = writeln!(out, " ({})", file.display());
                        }
                        _ => {
                            let _ = writeln!(out);
                        }
                    }
                }
                if !named {
                    let _ = writeln!(out, "    <no symbol> (ip {:#x})", frame.ip());
                }
            }
        }
        Ok(out)
    }

    #[cfg(not(all(target_os = "linux", feature = "stack-dumps")))]
    pub fn capture() -> Result<String, String> {
        Err("built without the `stack-dumps` feature on Linux".to_string())
    }

    /// Become the tracer child and exit. Only correct when the marker is set.
    #[cfg(all(target_os = "linux", feature = "stack-dumps"))]
    pub fn run_as_child() -> ! {
        // A failure here is the child's alone: the parent sees the trace fail
        // and degrades to identity-and-duration. Exiting non-zero would make
        // that clearer, but the parent already reports the error it got.
        if let Err(e) = rstack_self::child() {
            eprintln!("watchdog: stack-dump child failed: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    #[cfg(not(all(target_os = "linux", feature = "stack-dumps")))]
    pub fn run_as_child() -> ! {
        std::process::exit(0);
    }
}

/// Whether a stuck call will actually produce thread stacks in this process.
///
/// Three things must all hold: the `stack-dumps` feature, Linux, and a binary
/// that called [`install_stack_dump_hook`].
pub fn stack_dumps_available() -> bool {
    stacks::compiled_in() && stacks::hook_installed()
}

/// Enable thread-stack dumps for this binary. Call it **first thing in
/// `main`**, before parsing arguments or opening a database.
///
/// Two jobs, and the order matters. If this process *is* the tracer child, it
/// traces its parent and exits without ever returning. Otherwise it records
/// that the hook exists, which is what unlocks dumping at all.
///
/// Calling it late is not a style nit: everything `main` does before it runs
/// also runs in every child, so a hook placed after the database opens means
/// each dump opens the database again.
///
/// Safe to call from a binary built without the feature — then it does
/// nothing, and [`stack_dumps_available`] stays false.
pub fn install_stack_dump_hook() {
    if std::env::var_os(stacks::CHILD_MARKER_ENV).is_some() {
        stacks::run_as_child();
    }
    stacks::mark_hook_installed();
}

/// What the watchdog reports about itself, for `remind_me_server_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchdogStatus {
    pub enabled: bool,
    /// `None` when disabled.
    pub threshold_seconds: Option<f64>,
    pub calls_in_flight: usize,
}

#[derive(Debug)]
struct InflightCall {
    tool: String,
    started: Instant,
    /// Set once this call has been reported, so a single stuck call produces
    /// one line rather than one per monitor wake-up.
    reported: bool,
}

#[derive(Debug, Default)]
struct State {
    inflight: HashMap<u64, InflightCall>,
    shutdown: bool,
}

/// A stuck-call watchdog over one set of in-flight calls.
///
/// Ordinary use is the process-wide instance behind [`arm`] / [`status`].
/// Constructing one directly is for tests, which need their own threshold and
/// sink without racing every other test through a global.
pub struct Watchdog {
    threshold: Option<Duration>,
    state: Arc<(Mutex<State>, Condvar)>,
    next_id: AtomicU64,
    monitor_started: AtomicBool,
    sink: Sink,
}

impl Watchdog {
    /// Build a watchdog. `threshold` of `None` makes every operation a no-op.
    pub fn new(threshold: Option<Duration>, sink: Sink) -> Self {
        Self {
            threshold,
            state: Arc::new((Mutex::new(State::default()), Condvar::new())),
            next_id: AtomicU64::new(0),
            monitor_started: AtomicBool::new(false),
            sink,
        }
    }

    /// The process-wide watchdog, configured from the environment.
    pub fn global() -> &'static Watchdog {
        static GLOBAL: OnceLock<Watchdog> = OnceLock::new();
        GLOBAL.get_or_init(|| Watchdog::new(configured_threshold(), stderr_sink()))
    }

    /// Register a call as in flight. Dropping the returned guard ends it.
    ///
    /// When the watchdog is disabled the guard is inert, so callers need no
    /// enabled-check of their own.
    pub fn arm(&self, tool: &str) -> CallGuard<'_> {
        let Some(_) = self.threshold else {
            return CallGuard {
                watchdog: self,
                id: None,
            };
        };
        self.ensure_monitor();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (lock, condvar) = &*self.state;
        {
            let mut state = lock.lock().expect("watchdog state poisoned");
            state.inflight.insert(
                id,
                InflightCall {
                    tool: tool.to_string(),
                    started: Instant::now(),
                    reported: false,
                },
            );
        }
        // Wake the monitor so it recomputes its deadline: this call may be due
        // sooner than whatever it was already waiting for.
        condvar.notify_all();
        CallGuard {
            watchdog: self,
            id: Some(id),
        }
    }

    fn disarm(&self, id: u64) {
        let (lock, condvar) = &*self.state;
        {
            let mut state = lock.lock().expect("watchdog state poisoned");
            state.inflight.remove(&id);
        }
        condvar.notify_all();
    }

    /// Report the watchdog's own state.
    pub fn status(&self) -> WatchdogStatus {
        let (lock, _) = &*self.state;
        let inflight = lock.lock().map(|state| state.inflight.len()).unwrap_or(0);
        WatchdogStatus {
            enabled: self.threshold.is_some(),
            threshold_seconds: self.threshold.map(|t| t.as_secs_f64()),
            calls_in_flight: inflight,
        }
    }

    /// Start the monitor thread, once, on the first armed call.
    ///
    /// Lazily rather than at construction so a process that never serves a tool
    /// call — the CLI's one-shot subcommands, every test that only builds a
    /// server — never pays for a thread it will not use.
    fn ensure_monitor(&self) {
        if self.monitor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(threshold) = self.threshold else {
            return;
        };
        let state = Arc::clone(&self.state);
        let sink = Arc::clone(&self.sink);
        std::thread::Builder::new()
            .name("remind-me-watchdog".to_string())
            .spawn(move || monitor_loop(state, threshold, sink))
            // A watchdog that cannot start is a lost diagnostic, never a
            // reason to fail the server that was about to do real work.
            .map(|_| ())
            .unwrap_or_else(|e| eprintln!("watchdog: could not start monitor thread: {}", e));
    }

    /// Stop the monitor thread. Used by tests; the global instance runs for the
    /// life of the process.
    fn shutdown(&self) {
        let (lock, condvar) = &*self.state;
        if let Ok(mut state) = lock.lock() {
            state.shutdown = true;
        }
        condvar.notify_all();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn monitor_loop(state: Arc<(Mutex<State>, Condvar)>, threshold: Duration, sink: Sink) {
    let (lock, condvar) = &*state;
    let mut guard = lock.lock().expect("watchdog state poisoned");
    loop {
        if guard.shutdown {
            return;
        }

        // Report anything past the threshold, and find when the soonest
        // not-yet-due call becomes due.
        let now = Instant::now();
        let mut soonest: Option<Duration> = None;
        let mut due: Vec<StuckCall> = Vec::new();
        for call in guard.inflight.values_mut() {
            if call.reported {
                continue;
            }
            let elapsed = now.saturating_duration_since(call.started);
            if elapsed >= threshold {
                call.reported = true;
                due.push(StuckCall {
                    tool: call.tool.clone(),
                    elapsed,
                    stacks: None,
                });
            } else {
                let remaining = threshold - elapsed;
                soonest = Some(soonest.map_or(remaining, |s: Duration| s.min(remaining)));
            }
        }

        if !due.is_empty() {
            // Report without the lock held: a sink is caller-supplied code and
            // must not be able to block every arm/disarm in the process. The
            // stack capture below is the sharper version of the same rule --
            // it spawns a process and stops every thread, and doing that under
            // the state lock would wedge arm/disarm for its whole duration.
            drop(guard);

            // One capture per batch, attached to the first report. The dump is
            // process-wide, so if three calls went stuck together, three
            // copies of the same all-thread stack would be three times the
            // output and no extra information.
            if stack_dumps_available() {
                match stacks::capture() {
                    Ok(text) => due[0].stacks = Some(text),
                    // A failed capture must not cost the report that would
                    // have happened anyway. The commonest cause is a hardened
                    // host refusing `ptrace`, which is worth naming once.
                    Err(e) => eprintln!("watchdog: could not capture thread stacks: {e}"),
                }
            }

            for stuck in &due {
                sink(stuck);
            }
            guard = lock.lock().expect("watchdog state poisoned");
            continue;
        }

        guard = match soonest {
            Some(wait) => {
                condvar
                    .wait_timeout(guard, wait)
                    .expect("watchdog state poisoned")
                    .0
            }
            // Nothing pending: sleep until an arm or a shutdown wakes us.
            None => condvar.wait(guard).expect("watchdog state poisoned"),
        };
    }
}

/// Holds one call's registration. Dropping it disarms that call.
pub struct CallGuard<'a> {
    watchdog: &'a Watchdog,
    /// `None` when the watchdog is disabled — nothing was registered.
    id: Option<u64>,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.watchdog.disarm(id);
        }
    }
}

/// Register a call as in flight on the process-wide watchdog.
pub fn arm(tool: &str) -> CallGuard<'static> {
    Watchdog::global().arm(tool)
}

/// The process-wide watchdog's state, for `remind_me_server_status`.
pub fn status() -> WatchdogStatus {
    Watchdog::global().status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A sink that forwards every report to a channel the test can drain.
    fn channel_sink() -> (Sink, mpsc::Receiver<StuckCall>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        (
            Arc::new(move |stuck: &StuckCall| {
                let _ = tx.lock().expect("sink lock").send(stuck.clone());
            }),
            rx,
        )
    }

    /// Short enough to keep the suite fast, long enough that a fast call
    /// finishing inside it is not a race.
    const THRESHOLD: Duration = Duration::from_millis(120);

    #[test]
    fn a_call_that_exceeds_the_threshold_is_reported() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let _guard = watchdog.arm("remind_me_search");
        let stuck = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a call held past the threshold should be reported");
        assert_eq!(stuck.tool, "remind_me_search");
        assert!(stuck.elapsed >= THRESHOLD);
    }

    #[test]
    fn a_fast_call_is_not_reported() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        drop(watchdog.arm("remind_me_add"));

        // Wait past the threshold: if the disarm did not take, this is when a
        // spurious report would arrive.
        assert!(
            rx.recv_timeout(THRESHOLD * 4).is_err(),
            "a call that finished before the threshold must not be reported"
        );
    }

    /// The reference's reason for reference-counting: concurrent calls are
    /// normal, and the first one finishing must not silence the others.
    #[test]
    fn an_overlapping_call_is_still_watched_after_the_first_finishes() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let slow = watchdog.arm("remind_me_maintenance");
        drop(watchdog.arm("remind_me_add")); // finishes immediately

        let stuck = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the still-running call must still be reported");
        assert_eq!(stuck.tool, "remind_me_maintenance");
        drop(slow);
    }

    #[test]
    fn a_stuck_call_is_reported_once_not_once_per_wakeup() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let _guard = watchdog.arm("remind_me_search");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("first report");

        assert!(
            rx.recv_timeout(THRESHOLD * 4).is_err(),
            "one stuck call should produce one report, not a stream"
        );
    }

    #[test]
    fn a_disabled_watchdog_reports_nothing_and_counts_nothing() {
        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(None, sink);

        let _guard = watchdog.arm("remind_me_search");
        assert_eq!(watchdog.status().calls_in_flight, 0);
        assert!(!watchdog.status().enabled);
        assert!(
            rx.recv_timeout(THRESHOLD * 4).is_err(),
            "a disabled watchdog must never report"
        );
    }

    #[test]
    fn status_tracks_calls_in_flight() {
        let (sink, _rx) = channel_sink();
        let watchdog = Watchdog::new(Some(Duration::from_secs(60)), sink);

        assert_eq!(watchdog.status().calls_in_flight, 0);
        let first = watchdog.arm("a");
        assert_eq!(watchdog.status().calls_in_flight, 1);
        let second = watchdog.arm("b");
        assert_eq!(watchdog.status().calls_in_flight, 2);
        drop(second);
        assert_eq!(watchdog.status().calls_in_flight, 1);
        drop(first);
        assert_eq!(watchdog.status().calls_in_flight, 0);
    }

    #[test]
    fn status_reports_the_configured_threshold() {
        let (sink, _rx) = channel_sink();
        let watchdog = Watchdog::new(Some(Duration::from_secs(30)), sink);
        assert_eq!(watchdog.status().threshold_seconds, Some(30.0));

        let (sink, _rx) = channel_sink();
        let disabled = Watchdog::new(None, sink);
        assert_eq!(disabled.status().threshold_seconds, None);
    }

    /// The interlock that keeps a missing hook from becoming a second server.
    ///
    /// Capture re-executes `current_exe()`. This test binary never calls
    /// [`install_stack_dump_hook`] — a libtest harness has nowhere to put it —
    /// so nothing may be spawned, whether or not the feature is compiled in.
    /// If this ever fails, every armed call in the test suite is forking a
    /// copy of the test binary.
    #[test]
    fn without_the_hook_no_dump_is_attempted() {
        assert!(
            !stack_dumps_available(),
            "a binary that never installed the hook must never spawn a tracer"
        );

        let (sink, rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);
        let _guard = watchdog.arm("remind_me_search");

        let stuck = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the call should still be reported");
        assert_eq!(
            stuck.stacks, None,
            "no hook means no stacks -- and the report still happens regardless"
        );
    }

    /// A panic must not leave the watchdog permanently armed — the failure the
    /// reference's manual `arm()`/`disarm()` pairing is exposed to and this
    /// guard is not.
    ///
    /// Only `calls_in_flight` is asserted, deliberately. Asserting "and nothing
    /// was reported" fails here under parallel test load, and correctly so:
    /// unwinding prints a backtrace before the guard drops, which on a loaded
    /// machine takes longer than this suite's deliberately-short threshold. The
    /// resulting report is a true positive about the panic machinery, not a
    /// disarm that did not happen — and the disarm is what this test is about.
    #[test]
    fn a_panicking_call_still_disarms() {
        let (sink, _rx) = channel_sink();
        let watchdog = Watchdog::new(Some(THRESHOLD), sink);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = watchdog.arm("remind_me_search");
            panic!("boom");
        }));
        assert!(result.is_err(), "the test's own panic should propagate");

        assert_eq!(
            watchdog.status().calls_in_flight,
            0,
            "unwinding past the guard must still disarm the call"
        );
    }
}
