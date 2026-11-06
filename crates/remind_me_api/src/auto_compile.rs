//! The dashboard's "run with the local model" control: the automatic wiki
//! compile (`remind_me_core::wiki_compile`, card #747) started from a browser.
//!
//! A batch takes minutes, and this server answers one connection at a time
//! with a ten-second write deadline, so a run cannot live inside a request.
//! `POST /api/wiki/compile/auto` starts one on its own thread and returns at
//! once; `GET` on the same path reports whether it is still running and, once
//! it is not, what it did. The run takes its own short store handles (the
//! core loop never holds one while the model writes), so the dashboard keeps
//! working meanwhile.
//!
//! One run at a time, per server: a second start while one runs is refused
//! here. The core loop's own claim on the wiki root still stops a run started
//! elsewhere (the CLI, an MCP call) from overlapping it.

use remind_me_core::wiki_compile::{AutoOptions, AutoReport};
use remind_me_core::wiki_fs::Wiki;
use remind_me_core::Database;
use serde::Serialize;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs one automatic compile. The port the configured model sits behind, so
/// tests can drive the endpoint without one.
pub type Compiler =
    dyn Fn(&Wiki, &Database, &AutoOptions) -> Result<AutoReport, String> + Send + Sync;

/// Batches one start may ask for, at most.
pub const MAX_BATCHES: usize = 20;

/// The current or last run, as `GET /api/wiki/compile/auto` reports it.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct RunStatus {
    pub running: bool,
    /// Batches the run was asked for; 0 before the first run.
    pub max_batches: usize,
    /// Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    /// What the last finished run did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<AutoReport>,
    /// Why the last finished run could not run at all (no model configured,
    /// a store error). A run that stopped on a failed batch has a `report`
    /// with `stopped` set instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a start request did, with the status of the run it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// A new run, just started.
    Now(RunStatus),
    /// Refused: this run was already going.
    Busy(RunStatus),
}

/// Starts runs on their own thread and remembers the latest one.
pub struct AutoCompile {
    status: Arc<Mutex<RunStatus>>,
    compiler: Arc<Compiler>,
}

impl AutoCompile {
    pub fn new(compiler: Arc<Compiler>) -> Self {
        Self {
            status: Arc::new(Mutex::new(RunStatus::default())),
            compiler,
        }
    }

    /// The configured local model (`REMIND_ME_WIKI_LLM_URL` and `_MODEL`).
    pub fn configured() -> Self {
        Self::new(Arc::new(
            remind_me_core::wiki_llm::compile_with_configured_model,
        ))
    }

    pub fn status(&self) -> RunStatus {
        lock(&self.status).clone()
    }

    /// Start a run of `options.max_batches` batches, or refuse because one
    /// is running.
    pub fn start(&self, db: Arc<Database>, wiki: Wiki, options: AutoOptions) -> Started {
        let started = {
            let mut status = lock(&self.status);
            if status.running {
                return Started::Busy(status.clone());
            }
            *status = RunStatus {
                running: true,
                max_batches: options.max_batches,
                started_ms: Some(now_ms()),
                ..RunStatus::default()
            };
            status.clone()
        };
        let shared = Arc::clone(&self.status);
        let compiler = Arc::clone(&self.compiler);
        std::thread::spawn(move || {
            // A panic must not leave the status saying "running" for good.
            let outcome =
                std::panic::catch_unwind(AssertUnwindSafe(|| compiler(&wiki, &db, &options)))
                    .unwrap_or_else(
                        |_| Err("the compile panicked; see the server log".to_string()),
                    );
            let mut status = lock(&shared);
            status.running = false;
            status.finished_ms = Some(now_ms());
            match outcome {
                Ok(report) => status.report = Some(report),
                Err(why) => status.error = Some(why),
            }
        });
        Started::Now(started)
    }
}

/// The status, even if a run's thread panicked while holding it: the last
/// value written is still the best answer there is.
fn lock(status: &Mutex<RunStatus>) -> MutexGuard<'_, RunStatus> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
