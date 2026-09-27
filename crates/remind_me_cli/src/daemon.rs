//! `rusty-remind-me daemon`, and the client side of every other subcommand
//! when `REMIND_ME_DAEMON` is on (ADR-0023 §2).
//!
//! Every client falls back to opening the store in its own process, exactly
//! as before the daemon, whenever it cannot use one: none will start, a
//! different build is running, or this client's settings differ from the
//! daemon's. The fallback is announced on stderr, never silent.

use remind_me_api::ApiServer;
use remind_me_core::daemon::client::{self, ConnectError, DaemonConnection};
use remind_me_core::daemon::endpoint::Endpoint;
use remind_me_core::daemon::ops::{self, Op, OpReply};
use remind_me_core::daemon::server::{Daemon, Service, Start};
use remind_me_core::daemon::wire::Mode;
use remind_me_core::wiki_fs::Wiki;
use remind_me_core::Database;
use remind_me_mcp::daemon_proxy::DaemonProxy;
use remind_me_mcp::{Handler, McpServer};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::error::Error;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The dashboard API's own per-connection timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// What the daemon does with each request: the MCP server, the store
/// operations and the dashboard API, all over one database.
struct StoreService {
    db: Arc<Database>,
    mcp: McpServer,
    api: ApiServer,
}

impl Service for StoreService {
    fn mcp(&self, line: &str) -> Option<Value> {
        self.mcp.handle_line(line)
    }

    fn op(&self, op: &Op) -> OpReply {
        ops::execute(&self.db.store(), op)
    }

    fn http(&self, stream: &mut TcpStream) -> io::Result<()> {
        stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
        let served = self.api.serve_one(stream);
        let _ = stream.shutdown(Shutdown::Both);
        served
    }
}

/// `rusty-remind-me daemon [status|stop]`.
pub fn command(args: &[String], db_path: &Path) -> Result<()> {
    let endpoint = Endpoint::for_db(db_path);
    match args.first().map(String::as_str) {
        None | Some("run") => run(db_path, endpoint),
        Some("status") => status(&endpoint),
        Some("stop") => stop(&endpoint),
        Some(other) => {
            eprintln!("Unknown daemon command: {other}. Available: run (default), status, stop");
            std::process::exit(1);
        }
    }
}

/// Own the store until a client asks this daemon to stop.
fn run(db_path: &Path, endpoint: Endpoint) -> Result<()> {
    // Lock before opening: a second daemon must stand down without running
    // migrations against a store the first one owns.
    let locked = match Daemon::start(endpoint)? {
        Start::Started(locked) => locked,
        Start::AlreadyRunning => {
            eprintln!(
                "rusty-remind-me daemon: already running for {}",
                db_path.display()
            );
            return Ok(());
        }
    };
    let db = Arc::new(Database::open(db_path)?);
    // Published only now, with any migration done, so no client connects to
    // a daemon that cannot answer yet.
    let daemon = locked.listen()?;
    // The background loops every long-lived subcommand used to start for
    // itself now run here once, however many clients there are.
    let scheduler = remind_me_core::scheduler::start_scheduler_for(&db.store());
    let watcher = remind_me_core::watcher::start_watcher_for(&db.store());
    let nudge = remind_me_core::promotion::start_nudge_for(&db.store());
    let mut sync = remind_me_core::sync::SyncWorker::from_env(db_path.to_path_buf());

    let wiki = Wiki::from_env();
    let service = StoreService {
        mcp: McpServer::shared(Arc::clone(&db), wiki.clone()),
        api: ApiServer::shared(Arc::clone(&db), wiki),
        db,
    };
    eprintln!(
        "rusty-remind-me daemon: serving {} on 127.0.0.1:{} (pid {})",
        db_path.display(),
        daemon.info().port,
        daemon.info().pid
    );
    let served = daemon.serve(Arc::new(service));

    if let Some(scheduler) = scheduler {
        scheduler.stop();
    }
    if let Some(watcher) = watcher {
        watcher.stop();
    }
    if let Some(nudge) = nudge {
        nudge.stop();
    }
    if let Some(sync) = sync.as_mut() {
        sync.stop();
    }
    Ok(served?)
}

fn status(endpoint: &Endpoint) -> Result<()> {
    match client::connect(endpoint, Mode::Control) {
        Ok(mut control) => {
            let status: Value = control.call(&Op::Status)?.into_result()?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        Err(ConnectError::NotRunning) => {
            println!("not running");
            std::process::exit(1);
        }
        Err(e) => Err(e.into()),
    }
}

fn stop(endpoint: &Endpoint) -> Result<()> {
    match client::connect(endpoint, Mode::Control) {
        Ok(mut control) => {
            control.call(&Op::Shutdown)?.into_result::<Value>()?;
            println!("stopped");
            Ok(())
        }
        Err(ConnectError::NotRunning) => {
            println!("not running");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// This executable, which is what a client starts as the daemon.
fn this_exe() -> io::Result<PathBuf> {
    std::env::current_exe()
}

fn announce_fallback(e: &dyn std::fmt::Display) {
    eprintln!(
        "rusty-remind-me: not using the store daemon: {e}. Opening the store in this process."
    );
}

/// A relay to the daemon's MCP server, or `None` to serve in-process.
pub fn mcp_proxy(db_path: &Path) -> Option<DaemonProxy> {
    if !remind_me_core::daemon::enabled() {
        return None;
    }
    let exe = match this_exe() {
        Ok(exe) => exe,
        Err(e) => {
            announce_fallback(&e);
            return None;
        }
    };
    match DaemonProxy::connect(Endpoint::for_db(db_path), exe) {
        Ok(proxy) => Some(proxy),
        Err(e) => {
            announce_fallback(&e);
            None
        }
    }
}

/// Where the CLI's store operations run.
pub enum Store {
    Local(Database),
    Daemon(DaemonConnection),
}

impl Store {
    /// The daemon when it is enabled and usable, otherwise the store itself.
    pub fn open(db_path: &Path) -> Result<Self> {
        if remind_me_core::daemon::enabled() {
            let endpoint = Endpoint::for_db(db_path);
            let connected = this_exe().map_err(|e| e.to_string()).and_then(|exe| {
                client::connect_or_start(&endpoint, &exe, Mode::Op).map_err(|e| e.to_string())
            });
            match connected {
                Ok(connection) => return Ok(Store::Daemon(connection)),
                Err(e) => announce_fallback(&e),
            }
        }
        Ok(Store::Local(Database::open(db_path)?))
    }

    /// Run `op`, wherever the store is.
    pub fn call<T: DeserializeOwned>(&mut self, op: Op) -> Result<T> {
        let reply = match self {
            Store::Local(db) => ops::execute(&db.store(), &op),
            Store::Daemon(connection) => connection.call(&op)?,
        };
        Ok(reply.into_result()?)
    }
}

/// Serve the dashboard API on `addr` by relaying each connection to the
/// daemon. `Ok(false)` when the daemon is not usable and the caller should
/// serve in-process.
pub fn serve_api_via_daemon(db_path: &Path, addr: &str) -> Result<bool> {
    if !remind_me_core::daemon::enabled() {
        return Ok(false);
    }
    let endpoint = Endpoint::for_db(db_path);
    // One probe up front, starting the daemon if need be, so a refusal falls
    // back before this process binds the port.
    let probe = this_exe().map_err(|e| e.to_string()).and_then(|exe| {
        client::connect_or_start(&endpoint, &exe, Mode::Op).map_err(|e| e.to_string())
    });
    if let Err(e) = probe {
        announce_fallback(&e);
        return Ok(false);
    }
    let listener = TcpListener::bind(addr)?;
    println!(
        "REST API server listening on http://{} (via the store daemon)",
        addr
    );
    for browser in listener.incoming() {
        let Ok(browser) = browser else { continue };
        let endpoint = endpoint.clone();
        std::thread::spawn(move || {
            let relayed = this_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| {
                    client::connect_or_start(&endpoint, &exe, Mode::Http).map_err(|e| e.to_string())
                })
                .and_then(|daemon| {
                    client::relay(browser, daemon.into_stream()).map_err(|e| e.to_string())
                });
            if let Err(e) = relayed {
                eprintln!("rusty-remind-me api: could not reach the store daemon: {e}");
            }
        });
    }
    Ok(true)
}
