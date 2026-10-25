//! The secret gate's rate limiter keys anonymous callers by peer address
//! (issue #920). Served without connect-info, every such caller shared one
//! `ip:unknown` bucket, so one noisy client locked out everyone else.
//!
//! Kept in its own test binary because the limiter is process-global: any
//! other test hitting the gate from loopback would share these buckets.
//!
//! Linux only: it needs a second loopback source address (127.0.0.2), which
//! Linux routes by default and macOS does not.
#![cfg(target_os = "linux")]

mod common;
#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use remind_me_core::rate_limit::{
    RATE_LIMIT_ENABLED_ENV, RATE_LIMIT_REQUESTS_ENV, RATE_LIMIT_WINDOW_ENV,
};
use remind_me_core::Database;
use remind_me_mcp::McpServer;
use reqwest::StatusCode;

/// Small enough to exhaust quickly; the window outlasts the test.
const LIMIT: u32 = 3;

async fn spawn_server() -> SocketAddr {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router = remind_me_remote::build_router(mcp, "test-token".to_string(), None).unwrap();
    common::serve(router)
}

fn client_from(source: Ipv4Addr) -> reqwest::Client {
    reqwest::Client::builder()
        .local_address(IpAddr::V4(source))
        .build()
        .unwrap()
}

/// An unauthenticated `/mcp` request from the given client: 401 when
/// admitted, 429 when limited. The limit applies before any credential is
/// checked, so a refused credential still counts against the bucket.
async fn status(client: &reqwest::Client, addr: SocketAddr) -> StatusCode {
    client
        .get(format!("http://{addr}/mcp"))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn two_peers_get_separate_rate_limit_buckets() {
    // Pinned rather than inherited, before the process-global limiter is
    // first built, so an operator's REMIND_ME_RATE_LIMIT_* cannot skew it.
    test_env::set_var(RATE_LIMIT_ENABLED_ENV, "1");
    test_env::set_var(RATE_LIMIT_REQUESTS_ENV, LIMIT.to_string());
    test_env::set_var(RATE_LIMIT_WINDOW_ENV, "3600");
    let addr = spawn_server().await;
    let noisy = client_from(Ipv4Addr::new(127, 0, 0, 1));
    let quiet = client_from(Ipv4Addr::new(127, 0, 0, 2));

    for i in 0..LIMIT {
        assert_eq!(
            status(&noisy, addr).await,
            StatusCode::UNAUTHORIZED,
            "request {i} not admitted as an unauthenticated 401 while under the limit"
        );
    }
    assert_eq!(
        status(&noisy, addr).await,
        StatusCode::TOO_MANY_REQUESTS,
        "the noisy peer's own bucket should be exhausted"
    );

    assert_eq!(
        status(&quiet, addr).await,
        StatusCode::UNAUTHORIZED,
        "a second peer must not share the first peer's bucket"
    );
}
