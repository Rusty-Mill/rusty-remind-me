//! The server under test, served by `remind_me_remote::serve` on a
//! `rusty_tokio` runtime, as `run_blocking` serves it. The tests' own tokio
//! runtime only drives the HTTP client.

use std::net::SocketAddr;
use std::sync::OnceLock;

use rusty_axum::Router;
use rusty_tokio::io::TcpListener;
use rusty_tokio::runtime::Runtime;

/// One runtime for the whole test binary, not one per test.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        rusty_tokio::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("server runtime")
    })
}

/// Serves `router` on an ephemeral loopback port for the rest of the test
/// binary's life, and returns its address.
pub fn serve(router: Router) -> SocketAddr {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let server = {
        let _entered = runtime().enter();
        let listener = TcpListener::from_std(std_listener).expect("register listener");
        remind_me_remote::serve(listener, router).expect("serve")
    };
    let addr = server.local_addr();
    // Dropping the server would stop it; it lives as long as the test binary.
    std::mem::forget(server);
    addr
}
