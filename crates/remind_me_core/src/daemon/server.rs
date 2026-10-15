//! The daemon's listener: authentication, sessions and framing.
//!
//! What each request actually does is a [`Service`]'s business. The real one
//! lives in the CLI, where the MCP server and the dashboard API are both in
//! reach; tests here use a fake.

use super::endpoint::{DaemonInfo, Endpoint};
use super::ops::{Op, OpReply};
use super::settings::{self, Fingerprint};
use super::wire::{self, Hello, Mode, Refusal, Reply};
use rusty_crypto_key::constant_time_eq;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A client has this long to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections may be waiting on their hello at once.
const MAX_PENDING_HELLOS: usize = 64;

/// What the daemon does with each request.
pub trait Service: Send + Sync + 'static {
    /// Answer one MCP JSON-RPC line, `None` for a notification.
    fn mcp(&self, line: &str) -> Option<Value>;
    /// Run one store operation.
    fn op(&self, op: &Op) -> OpReply;
    /// Serve one HTTP exchange on `stream`.
    fn http(&self, stream: &mut TcpStream) -> io::Result<()>;
}

/// The outcome of [`Daemon::start`].
pub enum Start {
    Started(Locked),
    /// Another daemon holds this database's lock.
    AlreadyRunning,
}

/// A daemon holding its store's lock, not yet reachable.
///
/// The store is opened between [`Daemon::start`] and [`Locked::listen`]:
/// only the lock holder may migrate it, and clients find the daemon only
/// once it can answer them.
pub struct Locked {
    endpoint: Endpoint,
    lock: File,
}

/// A bound, published daemon, not yet serving.
pub struct Daemon {
    listener: TcpListener,
    endpoint: Endpoint,
    info: DaemonInfo,
    token: String,
    settings: Fingerprint,
    stopping: Arc<AtomicBool>,
    // Held, never read: the lock lasts as long as the file stays open.
    _lock: File,
}

struct Shared {
    info: DaemonInfo,
    token: String,
    settings: Fingerprint,
    stopping: Arc<AtomicBool>,
    shutdown_replies: Mutex<Vec<TcpStream>>,
}

impl Locked {
    /// Bind a loopback port and publish it with a fresh token.
    pub fn listen(self) -> io::Result<Daemon> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let info = DaemonInfo {
            pid: std::process::id(),
            port: listener.local_addr()?.port(),
            build: wire::build_id(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        let token = self.endpoint.publish(&info)?;
        Ok(Daemon {
            listener,
            endpoint: self.endpoint,
            info,
            token,
            settings: settings::fingerprint(),
            stopping: Arc::new(AtomicBool::new(false)),
            _lock: self.lock,
        })
    }
}

impl Daemon {
    /// Take the store's lock.
    pub fn start(endpoint: Endpoint) -> io::Result<Start> {
        Ok(match endpoint.try_lock()? {
            Some(lock) => Start::Started(Locked { endpoint, lock }),
            None => Start::AlreadyRunning,
        })
    }

    pub fn info(&self) -> &DaemonInfo {
        &self.info
    }

    /// Serve until a client sends [`Op::Shutdown`].
    pub fn serve(self, service: Arc<dyn Service>) -> io::Result<()> {
        self.serve_with_cleanup(service, || {})
    }

    /// Serve, drain connection workers and run `cleanup` before acknowledging
    /// shutdown. `cleanup` must stop and drop any background owners of the store.
    pub fn serve_with_cleanup(
        self,
        service: Arc<dyn Service>,
        cleanup: impl FnOnce(),
    ) -> io::Result<()> {
        let shared = Arc::new(Shared {
            info: self.info.clone(),
            token: self.token,
            settings: self.settings,
            stopping: Arc::clone(&self.stopping),
            shutdown_replies: Mutex::new(Vec::new()),
        });
        let pending_hellos = Arc::new(AtomicUsize::new(0));
        let mut workers: Vec<(Weak<TcpStream>, std::thread::JoinHandle<()>)> = Vec::new();
        let mut worker_failed = false;
        for stream in self.listener.incoming() {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            // Connections that have not yet said hello cost a thread and are
            // open to any local process, so only so many are let in at once.
            // Past hello, connections are not counted.
            let Some(pending) = PendingHello::admit(&pending_hellos) else {
                continue;
            };
            // Reap completed sessions during normal operation, rather than
            // retaining every connection for the lifetime of the daemon.
            let mut index = 0;
            while index < workers.len() {
                if workers[index].1.is_finished() {
                    worker_failed |= workers.swap_remove(index).1.join().is_err();
                } else {
                    index += 1;
                }
            }
            let socket = match stream.try_clone() {
                Ok(socket) => Arc::new(socket),
                Err(e) => {
                    eprintln!("rusty-remind-me daemon: could not track connection: {e}");
                    continue;
                }
            };
            let shared = Arc::clone(&shared);
            let service = Arc::clone(&service);
            let tracked_socket = Arc::downgrade(&socket);
            let worker = std::thread::spawn(move || {
                // Only the worker owns this socket. Tracking must not keep an
                // HTTP response or a panicked connection open after it ends.
                let _socket = socket;
                if let Err(e) = serve_connection(stream, pending, &shared, service.as_ref()) {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("rusty-remind-me daemon: connection ended: {e}");
                    }
                }
            });
            workers.push((tracked_socket, worker));
        }
        drop(self.listener);
        // Interrupt idle sessions on both platforms. Shutdown requesters have
        // already left their read loop and need their socket for the final reply.
        {
            let replies = shared
                .shutdown_replies
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for (socket, _) in &workers {
                let Some(socket) = socket.upgrade() else {
                    continue;
                };
                if !replies
                    .iter()
                    .any(|reply| reply.peer_addr().ok() == socket.peer_addr().ok())
                {
                    let _ = socket.shutdown(Shutdown::Both);
                }
            }
        }
        for (_, worker) in workers {
            worker_failed |= worker.join().is_err();
        }
        drop(service);
        cleanup();
        self.endpoint.withdraw(self.info.pid);
        drop(self._lock);
        let reply = if worker_failed {
            OpReply::Err("a daemon connection worker panicked during its lifetime".into())
        } else {
            OpReply::Ok(json!({"shutdown_complete": true}))
        };
        for mut writer in shared
            .shutdown_replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            // A client may have timed out; still notify the other requesters.
            if let Err(e) = wire::write_line(&mut writer, &reply) {
                eprintln!("rusty-remind-me daemon: shutdown reply failed: {e}");
            }
        }
        if worker_failed {
            return Err(io::Error::other("a daemon connection worker panicked"));
        }
        Ok(())
    }
}

/// One connection that has not yet finished its hello.
///
/// Releases its slot on drop, so every way out of the hello frees it.
struct PendingHello(Arc<AtomicUsize>);

impl PendingHello {
    /// A slot, or `None` while [`MAX_PENDING_HELLOS`] are already taken.
    fn admit(count: &Arc<AtomicUsize>) -> Option<Self> {
        if count.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING_HELLOS {
            count.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(count)))
    }
}

impl Drop for PendingHello {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

// Windows does not reliably interrupt a synchronous read on another thread
// when a cloned socket is shut down. Retry read timeouts without discarding a
// partially received line, checking cancellation between reads instead.
struct ConnectionReader {
    stream: TcpStream,
    stopping: Arc<AtomicBool>,
    hello_deadline: Option<Instant>,
}

impl Read for ConnectionReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return Ok(0);
            }
            if self
                .hello_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.read(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue
                }
                result => return result,
            }
        }
    }
}

fn serve_connection(
    stream: TcpStream,
    pending: PendingHello,
    shared: &Shared,
    service: &dyn Service,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(HELLO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(ConnectionReader {
        stream,
        stopping: Arc::clone(&shared.stopping),
        hello_deadline: Some(Instant::now() + HELLO_TIMEOUT),
    });
    let Some(hello) = wire::read_line_max::<Hello>(&mut reader, wire::MAX_HELLO_LINE)? else {
        return Ok(());
    };
    if let Some(refusal) = refuse(&hello, shared) {
        return wire::write_line(&mut writer, &Reply::Refused(refusal));
    }
    drop(pending);
    wire::write_line(&mut writer, &Reply::Welcome)?;
    reader.get_mut().hello_deadline = None;
    super::session::enter(hello.session);

    match hello.mode {
        Mode::Mcp => serve_mcp(reader, writer, service),
        Mode::Op => serve_ops(reader, writer, shared, Some(service)),
        Mode::Control => serve_ops(reader, writer, shared, None),
        Mode::Http => {
            debug_assert!(reader.buffer().is_empty());
            service.http(&mut writer)
        }
    }
}

/// Why `hello` is turned away, if it is.
fn refuse(hello: &Hello, shared: &Shared) -> Option<Refusal> {
    if !constant_time_eq(hello.token.as_bytes(), shared.token.as_bytes()) {
        return Some(Refusal::Token);
    }
    if hello.mode == Mode::Control {
        return None;
    }
    if hello.protocol != wire::PROTOCOL {
        return Some(Refusal::Protocol {
            daemon: wire::PROTOCOL,
        });
    }
    if hello.build != shared.info.build {
        return Some(Refusal::Build {
            daemon: shared.info.build.clone(),
        });
    }
    let differing = settings::differences(&hello.settings, &shared.settings);
    (!differing.is_empty()).then_some(Refusal::Settings { differing })
}

fn serve_mcp(reader: impl BufRead, mut writer: TcpStream, service: &dyn Service) -> io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        let reply = service.mcp(&line).unwrap_or(Value::Null);
        wire::write_line(&mut writer, &reply)?;
    }
    Ok(())
}

fn serve_ops(
    mut reader: impl BufRead,
    mut writer: TcpStream,
    shared: &Shared,
    service: Option<&dyn Service>,
) -> io::Result<()> {
    while let Some(op) = wire::read_line::<Op>(&mut reader)? {
        let reply = match op {
            Op::Status => OpReply::Ok(json!({
                "pid": shared.info.pid,
                "port": shared.info.port,
                "started_at": shared.info.started_at,
            })),
            Op::Shutdown => {
                shared
                    .shutdown_replies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(writer);
                shared.stopping.store(true, Ordering::SeqCst);
                // Wake the accept loop so it sees the flag.
                TcpStream::connect((Ipv4Addr::LOCALHOST, shared.info.port))?;
                return Ok(());
            }
            op => match service {
                Some(service) => service.op(&op),
                None => OpReply::Err("a control connection only runs status and shutdown".into()),
            },
        };
        wire::write_line(&mut writer, &reply)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::client::{self, ConnectError};
    use super::*;
    use std::io::Write;

    #[test]
    fn pending_hellos_are_capped_and_a_dropped_slot_is_reusable() {
        let count = Arc::new(AtomicUsize::new(0));
        let held: Vec<_> = (0..MAX_PENDING_HELLOS)
            .map(|_| PendingHello::admit(&count).expect("under the cap"))
            .collect();
        assert!(PendingHello::admit(&count).is_none());
        assert_eq!(count.load(Ordering::SeqCst), MAX_PENDING_HELLOS);
        drop(held);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(PendingHello::admit(&count).is_some());
    }

    #[test]
    fn shutdown_waits_for_cleanup_and_releases_the_store_with_idle_sessions() {
        let dir = std::env::temp_dir().join(format!("rrm_shutdown_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("memory.db");
        let endpoint = Endpoint::for_db(&db_path);
        let Start::Started(locked) = Daemon::start(endpoint.clone()).unwrap() else {
            panic!("fresh endpoint");
        };
        let db = crate::Database::open(&db_path).unwrap();
        let daemon = locked.listen().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            daemon.serve_with_cleanup(Arc::new(Echo), move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                drop(db);
            })
        });
        let _idle = client::connect(&endpoint, Mode::Mcp).unwrap();
        let _pending_hello =
            TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.read_info().unwrap().port)).unwrap();
        let control = client::connect(&endpoint, Mode::Control).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            done_tx
                .send(control.shutdown(Duration::from_secs(5)))
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(
            endpoint.try_lock().unwrap().is_none(),
            "cleanup still owns the daemon lock"
        );
        assert!(
            crate::Database::open(&db_path).is_err(),
            "store remains held during cleanup"
        );
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        // No join or retry before reopening: success itself is the barrier.
        let reopened = crate::Database::open(&db_path).unwrap();
        drop(reopened);
        client.join().unwrap();
        server.join().unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stalled_cleanup_times_out_without_reporting_success() {
        let endpoint = temp_endpoint();
        let Start::Started(locked) = Daemon::start(endpoint.clone()).unwrap() else {
            panic!("fresh endpoint");
        };
        let daemon = locked.listen().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            daemon.serve_with_cleanup(Arc::new(Echo), move || {
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        });
        let control = client::connect(&endpoint, Mode::Control).unwrap();
        let err = control.shutdown(Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("store may still be in use"));
        assert!(endpoint.try_lock().unwrap().is_none());
        release_tx.send(()).unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn a_panicked_worker_closes_its_session_and_shutdown_reports_failure() {
        struct Panics;
        impl Service for Panics {
            fn mcp(&self, _: &str) -> Option<Value> {
                panic!("injected worker failure");
            }
            fn op(&self, op: &Op) -> OpReply {
                Echo.op(op)
            }
            fn http(&self, stream: &mut TcpStream) -> io::Result<()> {
                Echo.http(stream)
            }
        }
        let endpoint = temp_endpoint();
        let Start::Started(locked) = Daemon::start(endpoint.clone()).unwrap() else {
            panic!("fresh endpoint");
        };
        let daemon = locked.listen().unwrap();
        let server = std::thread::spawn(move || daemon.serve(Arc::new(Panics)));
        let mut stream = client::connect(&endpoint, Mode::Mcp).unwrap().into_stream();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(b"panic\n").unwrap();
        assert_eq!(
            stream.read(&mut [0]).unwrap(),
            0,
            "panic closes the session"
        );
        let control = client::connect(&endpoint, Mode::Control).unwrap();
        let err = control.shutdown(Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("worker panicked"), "{err}");
        assert!(server.join().unwrap().is_err());
    }

    /// Echoes what it is given, and reports the calling session's client.
    struct Echo;

    impl Service for Echo {
        fn mcp(&self, line: &str) -> Option<Value> {
            let request: Value = serde_json::from_str(line).ok()?;
            request.get("id")?;
            Some(json!({
                "echo": request,
                "client": super::super::session::var("REMIND_ME_CLIENT"),
            }))
        }

        fn op(&self, op: &Op) -> OpReply {
            OpReply::Ok(serde_json::to_value(op).unwrap())
        }

        fn http(&self, stream: &mut TcpStream) -> io::Result<()> {
            let mut request = [0u8; 4];
            stream.read_exact(&mut request)?;
            stream.write_all(b"pong")
        }
    }

    fn temp_endpoint() -> Endpoint {
        let dir = std::env::temp_dir().join(format!(
            "rrm_daemon_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        Endpoint::for_db(&dir.join("memory.db"))
    }

    fn running(endpoint: &Endpoint) -> std::thread::JoinHandle<io::Result<()>> {
        let Start::Started(locked) = Daemon::start(endpoint.clone()).unwrap() else {
            panic!("fresh endpoint must start");
        };
        let daemon = locked.listen().unwrap();
        std::thread::spawn(move || daemon.serve(Arc::new(Echo)))
    }

    fn stop(endpoint: &Endpoint, handle: std::thread::JoinHandle<io::Result<()>>) {
        let store = client::connect(endpoint, Mode::Control).unwrap();
        store.shutdown(Duration::from_secs(5)).unwrap();
        handle.join().unwrap().unwrap();
        assert!(endpoint.read_info().is_none(), "info withdrawn on exit");
    }

    #[test]
    fn a_second_daemon_on_the_same_store_stands_down() {
        let endpoint = temp_endpoint();
        let handle = running(&endpoint);
        assert!(matches!(
            Daemon::start(endpoint.clone()).unwrap(),
            Start::AlreadyRunning
        ));
        stop(&endpoint, handle);
    }

    #[test]
    fn every_mode_round_trips() {
        let endpoint = temp_endpoint();
        let handle = running(&endpoint);

        let mut mcp = client::connect(&endpoint, Mode::Mcp).unwrap();
        let reply = mcp
            .call_mcp(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)
            .unwrap();
        assert_eq!(reply.unwrap()["echo"]["id"], 3);
        // A notification gets its `null` and the stream stays in step.
        assert!(mcp
            .call_mcp(r#"{"jsonrpc":"2.0","method":"x"}"#)
            .unwrap()
            .is_none());
        assert!(mcp.call_mcp(r#"{"id":4}"#).unwrap().is_some());

        let mut ops = client::connect(&endpoint, Mode::Op).unwrap();
        let status = ops.call(&Op::Status).unwrap();
        let status: Value = status.into_result().unwrap();
        assert_eq!(status["pid"], std::process::id());
        let echoed: Value = ops.call(&Op::Stats).unwrap().into_result().unwrap();
        assert_eq!(echoed["op"], "stats");

        let mut http = client::connect(&endpoint, Mode::Http)
            .unwrap()
            .into_stream();
        http.write_all(b"ping").unwrap();
        let mut answer = String::new();
        http.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "pong");

        stop(&endpoint, handle);
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let endpoint = temp_endpoint();
        let handle = running(&endpoint);
        let real = endpoint.read_token().unwrap();
        std::fs::write(endpoint.token_path(), "not-it").unwrap();
        let err = client::connect(&endpoint, Mode::Op).err().unwrap();
        assert!(
            matches!(err, ConnectError::Refused(Refusal::Token)),
            "{err}"
        );
        std::fs::write(endpoint.token_path(), real).unwrap();
        stop(&endpoint, handle);
    }

    #[test]
    fn a_stale_info_file_reads_as_not_running() {
        let endpoint = temp_endpoint();
        std::fs::create_dir_all(endpoint.lock_path().parent().unwrap()).unwrap();
        // Bind and drop, so the port is very likely closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        endpoint
            .publish(&DaemonInfo {
                pid: 1,
                port,
                build: "old".into(),
                started_at: "then".into(),
            })
            .unwrap();
        let err = client::connect(&endpoint, Mode::Op).err().unwrap();
        assert!(matches!(err, ConnectError::NotRunning), "{err}");
    }

    #[test]
    fn refusals_are_decided_in_order() {
        let shared = Shared {
            info: DaemonInfo {
                pid: 1,
                port: 1,
                build: "b".into(),
                started_at: "t".into(),
            },
            token: "tok".into(),
            settings: settings::fingerprint_of([("REMIND_ME_SYNC_SECRET".into(), "a".into())]),
            stopping: Arc::new(AtomicBool::new(false)),
            shutdown_replies: Mutex::new(Vec::new()),
        };
        let hello = |token: &str, build: &str, secret: &str| Hello {
            protocol: wire::PROTOCOL,
            token: token.into(),
            build: build.into(),
            mode: Mode::Op,
            session: Default::default(),
            settings: settings::fingerprint_of([("REMIND_ME_SYNC_SECRET".into(), secret.into())]),
        };
        assert_eq!(refuse(&hello("tok", "b", "a"), &shared), None);
        assert_eq!(
            refuse(&hello("bad", "x", "z"), &shared),
            Some(Refusal::Token)
        );
        assert!(matches!(
            refuse(&hello("tok", "x", "z"), &shared),
            Some(Refusal::Build { .. })
        ));
        assert_eq!(
            refuse(&hello("tok", "b", "z"), &shared),
            Some(Refusal::Settings {
                differing: vec!["REMIND_ME_SYNC_SECRET".into()]
            })
        );
        // Control needs only the token: stop must reach any daemon.
        let control = |token: &str| Hello {
            mode: Mode::Control,
            ..hello(token, "x", "z")
        };
        assert_eq!(refuse(&control("tok"), &shared), None);
        assert_eq!(refuse(&control("bad"), &shared), Some(Refusal::Token));
    }

    #[test]
    fn a_control_connection_runs_no_store_ops() {
        let endpoint = temp_endpoint();
        let handle = running(&endpoint);
        let mut control = client::connect(&endpoint, Mode::Control).unwrap();
        assert!(matches!(control.call(&Op::Stats).unwrap(), OpReply::Err(_)));
        let status: Value = control.call(&Op::Status).unwrap().into_result().unwrap();
        assert_eq!(status["port"], endpoint.read_info().unwrap().port);
        drop(control);
        stop(&endpoint, handle);
    }
}
