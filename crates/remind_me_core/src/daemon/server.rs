//! The daemon's listener: authentication, sessions and framing.
//!
//! What each request actually does is a [`Service`]'s business. The real one
//! lives in the CLI, where the MCP server and the dashboard API are both in
//! reach; tests here use a fake.

use super::endpoint::{DaemonInfo, Endpoint};
use super::ops::{Op, OpReply};
use super::settings::{self, Fingerprint};
use super::wire::{self, Hello, Mode, Refusal, Reply};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
        let shared = Arc::new(Shared {
            info: self.info.clone(),
            token: self.token,
            settings: self.settings,
            stopping: Arc::clone(&self.stopping),
        });
        let pending_hellos = Arc::new(AtomicUsize::new(0));
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
            let shared = Arc::clone(&shared);
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                if let Err(e) = serve_connection(stream, pending, &shared, service.as_ref()) {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("rusty-remind-me daemon: connection ended: {e}");
                    }
                }
            });
        }
        self.endpoint.withdraw(self.info.pid);
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

fn serve_connection(
    stream: TcpStream,
    pending: PendingHello,
    shared: &Shared,
    service: &dyn Service,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let Some(hello) = wire::read_line_max::<Hello>(&mut reader, wire::MAX_HELLO_LINE)? else {
        return Ok(());
    };
    if let Some(refusal) = refuse(&hello, shared) {
        return wire::write_line(&mut writer, &Reply::Refused(refusal));
    }
    drop(pending);
    wire::write_line(&mut writer, &Reply::Welcome)?;
    reader.get_ref().set_read_timeout(None)?;
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
                shared.stopping.store(true, Ordering::SeqCst);
                wire::write_line(&mut writer, &OpReply::Ok(Value::Null))?;
                // Wake the accept loop so it sees the flag.
                let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, shared.info.port));
                return writer.flush();
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
    use std::io::Read;

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
        let mut store = client::connect(endpoint, Mode::Control).unwrap();
        assert_eq!(store.call(&Op::Shutdown).unwrap(), OpReply::Ok(Value::Null));
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
