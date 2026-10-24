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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use remind_me_core::Database;
use remind_me_mcp::McpServer;
use reqwest::StatusCode;

async fn spawn_server() -> SocketAddr {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router = remind_me_remote::build_router(mcp, "test-token".to_string(), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(remind_me_remote::serve(listener, router));
    addr
}

fn client_from(source: Ipv4Addr) -> reqwest::Client {
    reqwest::Client::builder()
        .local_address(IpAddr::V4(source))
        .build()
        .unwrap()
}

/// An unauthenticated `/mcp` request from the given client; it is limited
/// before any credential is checked, so it still counts against the bucket.
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
    assert!(
        remind_me_core::rate_limit::rate_limit_enabled(),
        "rate limiting must be enabled for this test"
    );
    let addr = spawn_server().await;
    let noisy = client_from(Ipv4Addr::new(127, 0, 0, 1));
    let quiet = client_from(Ipv4Addr::new(127, 0, 0, 2));

    for i in 0..remind_me_core::rate_limit::configured_requests() {
        assert_ne!(
            status(&noisy, addr).await,
            StatusCode::TOO_MANY_REQUESTS,
            "request {i} limited while under the limit"
        );
    }
    assert_eq!(
        status(&noisy, addr).await,
        StatusCode::TOO_MANY_REQUESTS,
        "the noisy peer's own bucket should be exhausted"
    );

    assert_ne!(
        status(&quiet, addr).await,
        StatusCode::TOO_MANY_REQUESTS,
        "a second peer must not share the first peer's bucket"
    );
}
