//! [`rmcp::ServerHandler`] adapter over [`remind_me_mcp::McpServer`].
//!
//! # Why an adapter, not a raw JSON-RPC passthrough
//!
//! `rmcp` 3.0.1's Streamable HTTP transport (`StreamableHttpService<S, M>`,
//! in `transport::streamable_http_server::tower`) is generic over
//! `S: ServerHandler` — there is no constructor or trait that accepts a
//! caller-supplied `fn(&str) -> Option<Value>` and forwards raw JSON-RPC
//! bytes to it. The SDK owns protocol-level dispatch itself: it decodes
//! every request into a typed `ClientRequest` variant and calls the matching
//! typed `ServerHandler` method (`list_tools`, `call_tool`,
//! `read_resource`, ...), handling protocol version negotiation, the
//! 2026-07-28 MRTR/task extensions, and SSE/session framing itself. This was
//! confirmed by reading `handler/server.rs`'s `Service<RoleServer> for H`
//! blanket impl and the `StreamableHttpService::new` bound directly (both
//! vendored under `~/.cargo/registry/src/.../rmcp-3.0.1/`), not assumed from
//! the crate's public docs.
//!
//! So this adapter is the thin translation layer the module doc on `#85`
//! anticipated might be necessary: each `ServerHandler` method this crate
//! implements builds the same JSON-RPC envelope the stdio transport sends,
//! hands it to [`remind_me_mcp::McpServer::handle_request`] — the crate's
//! one, already-tested dispatch entry point, reused rather than
//! reimplemented — and deserializes the `result` field straight into the
//! matching `rmcp::model` type. That deserialization isn't hand-written
//! field-by-field mapping: `handle_request`'s JSON shapes (`{"tools": [...]}`,
//! `{"content": [...], "isError": ...}`, `{"contents": [...]}`, ...) already
//! match rmcp's own camelCase wire format for `ListToolsResult`,
//! `CallToolResult`, `ReadResourceResult`, etc., so `serde_json::from_value`
//! does the whole job.
//!
//! Only the methods `handle_request` already answers (`initialize` via the
//! default `get_info`-based impl, `tools/list`, `tools/call`,
//! `resources/list`, `resources/read`, `prompts/list`) are overridden here.
//! Everything else (`prompts/get`, subscriptions, tasks, completion) is left
//! at rmcp's own default — `method not found` or an empty result — which
//! matches the stdio transport's own coverage: `McpServer::handle_request`
//! doesn't answer those methods either.
//!
//! # Blocking dispatch, off the async runtime
//!
//! `handle_request` is synchronous and takes `Database`'s connection mutex.
//! Calling it directly from an async trait method would block a tokio
//! worker thread for the duration of a DB query; `tokio::task::spawn_blocking`
//! moves that work to the blocking pool instead, so one slow tool call
//! cannot stall unrelated connector traffic on the same runtime.

use std::sync::Arc;

use remind_me_mcp::McpServer;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, InitializeResult,
    ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

/// Adapts a synchronous [`McpServer`] to rmcp's async, typed `ServerHandler`.
///
/// Cheap to clone: `rmcp` builds one `S` per session via a service factory
/// closure, so this only wraps an `Arc`.
#[derive(Clone)]
pub struct RemindMeHandler {
    mcp: Arc<McpServer>,
}

impl RemindMeHandler {
    pub fn new(mcp: Arc<McpServer>) -> Self {
        Self { mcp }
    }

    /// Build the JSON-RPC envelope `handle_request` expects, dispatch it on
    /// the blocking pool, and return its `result` (or a protocol-level
    /// `McpError` if dispatch failed or produced no response at all — the
    /// latter should not happen for any method this adapter calls, since
    /// none of them are the fire-and-forget notification methods
    /// `handle_request` answers with `None`).
    async fn dispatch(&self, method: &'static str, params: Value) -> Result<Value, McpError> {
        let mcp = Arc::clone(&self.mcp);
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        })
        .to_string();

        let response = tokio::task::spawn_blocking(move || mcp.handle_request(&envelope))
            .await
            .map_err(|e| McpError::internal_error(format!("{method} task panicked: {e}"), None))?
            .ok_or_else(|| {
                McpError::internal_error(format!("{method} produced no response"), None)
            })?;

        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("dispatch error")
                .to_string();
            return Err(McpError::internal_error(message, Some(error.clone())));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// As [`Self::dispatch`], additionally deserializing `result` into a
    /// typed rmcp model struct.
    async fn dispatch_typed<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<T, McpError> {
        let result = self.dispatch(method, params).await?;
        serde_json::from_value(result)
            .map_err(|e| McpError::internal_error(format!("malformed {method} result: {e}"), None))
    }
}

impl ServerHandler for RemindMeHandler {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_prompts()
                .enable_prompts_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new(
            "rusty_remind_me",
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.dispatch_typed("tools/list", json!({})).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let params = json!({
            "name": request.name,
            "arguments": request
                .arguments
                .map(Value::Object)
                .unwrap_or_else(|| json!({})),
        });
        let result: CallToolResult = self.dispatch_typed("tools/call", params).await?;
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.dispatch_typed("resources/list", json!({})).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let params = json!({ "uri": request.uri });
        let result: ReadResourceResult = self.dispatch_typed("resources/read", params).await?;
        Ok(result.into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        self.dispatch_typed("prompts/list", json!({})).await
    }
}

// The per-method handlers above (`list_tools`, `call_tool`, `list_resources`,
// `read_resource`, `list_prompts`) each need a `RequestContext<RoleServer>`,
// whose `peer: Peer<RoleServer>` field has a crate-private constructor
// (`Peer::new` is `pub(crate)` in rmcp) — there is no public way to build
// one from outside the `rmcp` crate itself. Exercising them is therefore
// left to this crate's integration tests (`tests/`), which drive the real
// `StreamableHttpService` over HTTP; rmcp constructs the `RequestContext`
// internally as part of a real session, which also verifies the SSE/session
// framing these methods run inside end to end rather than in isolation.
#[cfg(test)]
mod tests {
    use super::*;
    use remind_me_core::Database;

    fn handler() -> RemindMeHandler {
        let db = Database::open_in_memory().unwrap();
        RemindMeHandler::new(Arc::new(McpServer::new(db)))
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn remind_me_handler_is_send_and_sync() {
        // `ServerHandler` requires `Send + Sync + 'static` outside the
        // `local` feature (which this crate does not enable) -- this both
        // documents that requirement and catches a regression at compile
        // time if `McpServer` ever grows a non-Send/Sync field.
        assert_send_sync::<RemindMeHandler>();
    }

    #[test]
    fn get_info_advertises_tools_resources_and_prompts() {
        let info = handler().get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
        assert!(info.capabilities.prompts.is_some());
        assert_eq!(info.server_info.name, "rusty_remind_me");
    }
}
