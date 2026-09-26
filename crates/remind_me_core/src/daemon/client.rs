//! Reaching the daemon from a client process.

use super::endpoint::Endpoint;
use super::ops::{Op, OpReply};
use super::session::Session;
use super::wire::{self, Hello, Mode, Refusal, Reply};
use serde_json::Value;
use std::fmt;
use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the daemon has to answer a hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a client waits for a daemon it started. Opening the database runs
/// any pending migration first, which can take a while on a large store.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const START_POLL: Duration = Duration::from_millis(50);

/// Why a client is not talking to a daemon.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing answered as a daemon should.
    NotRunning,
    /// A daemon answered and turned this client away.
    Refused(Refusal),
    /// Starting one failed.
    Start(io::Error),
    /// One was started and exited with an error before it was ready.
    Exited {
        status: std::process::ExitStatus,
        log: std::path::PathBuf,
    },
    /// One was started and never became ready.
    StartTimedOut,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NotRunning => write!(f, "no daemon is running"),
            ConnectError::Refused(refusal) => refusal.fmt(f),
            ConnectError::Start(e) => write!(f, "could not start the daemon: {e}"),
            ConnectError::Exited { status, log } => write!(
                f,
                "the daemon exited ({status}) before it was ready; see {}",
                log.display()
            ),
            ConnectError::StartTimedOut => write!(
                f,
                "the daemon did not become ready within {}s",
                START_TIMEOUT.as_secs()
            ),
        }
    }
}

impl std::error::Error for ConnectError {}

/// An open, welcomed connection.
pub struct DaemonConnection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl DaemonConnection {
    /// Send one MCP JSON-RPC line; `None` is the answer to a notification.
    pub fn call_mcp(&mut self, line: &str) -> io::Result<Option<Value>> {
        let line = line.trim_end_matches(['\r', '\n']);
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        let reply: Value = wire::read_line(&mut self.reader)?.ok_or_else(closed)?;
        Ok((!reply.is_null()).then_some(reply))
    }

    /// Run one [`Op`].
    pub fn call(&mut self, op: &Op) -> io::Result<OpReply> {
        wire::write_line(&mut self.writer, op)?;
        wire::read_line(&mut self.reader)?.ok_or_else(closed)
    }

    /// The raw stream, for an HTTP exchange.
    pub fn into_stream(self) -> TcpStream {
        debug_assert!(
            self.reader.buffer().is_empty(),
            "the daemon sends nothing between its welcome and the request"
        );
        self.writer
    }
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the daemon closed the connection",
    )
}

/// Connect to the running daemon, if there is one.
pub fn connect(endpoint: &Endpoint, mode: Mode) -> Result<DaemonConnection, ConnectError> {
    let info = endpoint.read_info().ok_or(ConnectError::NotRunning)?;
    let token = endpoint.read_token().ok_or(ConnectError::NotRunning)?;
    let hello = Hello {
        protocol: wire::PROTOCOL,
        token,
        build: wire::build_id(),
        mode,
        session: Session::from_env(),
        settings: super::settings::fingerprint(),
    };
    // Anything short of a well-formed reply means no daemon: a stale
    // `daemon.json` can name a port some other process now holds.
    handshake(info.port, &hello).map_err(|_| ConnectError::NotRunning)?
}

fn handshake(port: u16, hello: &Hello) -> io::Result<Result<DaemonConnection, ConnectError>> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    wire::write_line(&mut writer, hello)?;
    let reply: Reply = wire::read_line(&mut reader)?.ok_or_else(closed)?;
    reader.get_ref().set_read_timeout(None)?;
    Ok(match reply {
        Reply::Welcome => Ok(DaemonConnection { reader, writer }),
        Reply::Refused(refusal) => Err(ConnectError::Refused(refusal)),
    })
}

/// Connect to the daemon, starting one from `exe` if none is running.
pub fn connect_or_start(
    endpoint: &Endpoint,
    exe: &Path,
    mode: Mode,
) -> Result<DaemonConnection, ConnectError> {
    match connect(endpoint, mode) {
        Err(ConnectError::NotRunning) => {}
        other => return other,
    }
    let mut child = start(endpoint, exe).map_err(ConnectError::Start)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let outcome = loop {
        match connect(endpoint, mode) {
            Err(ConnectError::NotRunning) => {}
            other => break other,
        }
        // A daemon that exits cleanly lost a start race to another one,
        // which is still worth waiting for. One that fails will not answer.
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return Err(ConnectError::Exited {
                    status,
                    log: endpoint.log_path(),
                });
            }
        }
        if Instant::now() >= deadline {
            break Err(ConnectError::StartTimedOut);
        }
        std::thread::sleep(START_POLL);
    };
    // Reap it whenever it exits, so a long-lived client never keeps a zombie.
    std::thread::spawn(move || child.wait());
    outcome
}

/// Launch `exe daemon` detached, with its stderr appended to `daemon.log`.
///
/// It inherits this process's environment, so it runs under the settings of
/// the client that started it: exactly what the fingerprint compares later
/// clients against. Two clients starting one at once is harmless: the second
/// daemon finds the lock taken and exits.
fn start(endpoint: &Endpoint, exe: &Path) -> io::Result<std::process::Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(endpoint.log_path())?;
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    detach(&mut command);
    command.spawn()
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group, so a Ctrl-C meant for the client that started
    // it does not stop the daemon every other client shares.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    stop_inheriting_std_handles();
}

/// Keep this process's own stdin, stdout and stderr out of the daemon.
///
/// Windows hands a child every inheritable handle, not only the three it is
/// given. This client's standard handles are usually pipes from whatever
/// launched it (Claude Code, a test), and a daemon holding the write end of
/// one would keep that pipe open long after this client exits: its launcher
/// would wait for an end of output that never comes. Unix needs nothing like
/// this: a child's descriptors 0 to 2 are replaced, and std opens everything
/// else close-on-exec.
#[cfg(windows)]
fn stop_inheriting_std_handles() {
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle takes no pointers, and SetHandleInformation
        // only changes a flag on a handle this process owns; an invalid or
        // absent handle is skipped, and a failure leaves the flag as it was.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

/// Relay an HTTP exchange between `browser` and the daemon until either side
/// closes.
pub fn relay(browser: TcpStream, daemon: TcpStream) -> io::Result<()> {
    let mut to_daemon = daemon.try_clone()?;
    let mut from_browser = browser.try_clone()?;
    let upstream = std::thread::spawn(move || {
        let copied = io::copy(&mut from_browser, &mut to_daemon);
        let _ = to_daemon.shutdown(std::net::Shutdown::Write);
        copied
    });
    let mut from_daemon = daemon;
    let mut to_browser = browser;
    io::copy(&mut from_daemon, &mut to_browser)?;
    let _ = to_browser.shutdown(std::net::Shutdown::Both);
    let _ = upstream.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_endpoint() -> Endpoint {
        let dir = std::env::temp_dir().join(format!(
            "rrm_client_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Endpoint::for_db(&dir.join("memory.db"))
    }

    /// A daemon that dies on start is reported at once, not after the whole
    /// start timeout, so the client falls back promptly.
    #[cfg(unix)]
    #[test]
    fn a_daemon_that_fails_to_start_is_reported_promptly() {
        let started = Instant::now();
        let err = connect_or_start(&temp_endpoint(), Path::new("/bin/false"), Mode::Op)
            .err()
            .expect("nothing can answer");
        assert!(matches!(err, ConnectError::Exited { .. }), "{err}");
        assert!(
            started.elapsed() < START_TIMEOUT / 4,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn nothing_published_is_not_running() {
        let err = connect(&temp_endpoint(), Mode::Op).err().unwrap();
        assert!(matches!(err, ConnectError::NotRunning));
    }
}
