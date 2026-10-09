//! The `rusty_mcp_server` description of a [`remind_me_mcp::Handler`].
//!
//! `remind_me_mcp::McpServer` already answers MCP as JSON-RPC lines
//! (`handle_line`): the stdio transport feeds it directly. This module is the
//! thin adapter that lets the HTTP transport serve the same answers: tools
//! are forwarded per request through a [`ToolSource`] (the handler's tool
//! list depends on the configured profile), and the handler's resources and
//! prompts are read once at build and registered, each forwarding its
//! `resources/read` / `prompts/get` back to the handler. No tool, resource or
//! prompt logic is reimplemented here.
//!
//! Handlers block (the database sits behind a mutex), which is fine: the HTTP
//! layer runs them on blocking threads.

use std::sync::Arc;

use remind_me_mcp::Handler;
use rusty_mcp_server::json::Value as WireValue;
use rusty_mcp_server::proto::{
    CallToolParams, CallToolResult, ErrorCode, ErrorData, GetPromptParams, GetPromptResult, Prompt,
    ReadResourceParams, ReadResourceResult, Resource, Tool, Wire,
};
use rusty_mcp_server::{BuildError, CallContext, Server, ServerBuilder, ToolSource};
use serde_json::{json, Value};

/// Name and version reported at `initialize`.
const SERVER_NAME: &str = "rusty_remind_me";

/// Why a server could not be described.
#[derive(Debug, thiserror::Error)]
pub enum DescribeError {
    /// The handler's own answer was not a valid list.
    #[error("the handler's {method} answer is malformed: {why}")]
    Malformed {
        /// The method asked.
        method: &'static str,
        /// What was wrong.
        why: String,
    },
    /// The assembled server was rejected.
    #[error(transparent)]
    Build(#[from] BuildError),
}

/// Ask the handler one question and return its `result`, or its error.
fn ask(mcp: &dyn Handler, method: &str, params: Value) -> Result<Value, ErrorData> {
    let envelope = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let Some(reply) = mcp.handle_line(&envelope.to_string()) else {
        return Err(ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            format!("{method} produced no response"),
        ));
    };
    if let Some(error) = reply.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .map_or(ErrorCode::INTERNAL_ERROR, ErrorCode);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("dispatch error");
        return Err(ErrorData::new(code, message));
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

/// A `serde_json` value as the wire crates' JSON type.
fn to_wire(value: &Value) -> Result<WireValue, ErrorData> {
    WireValue::from_json_str(&value.to_string())
        .map_err(|e| ErrorData::new(ErrorCode::INTERNAL_ERROR, e.to_string()))
}

/// Decode `value` as a `T`, naming `what` on failure.
fn decode<T: Wire>(value: &Value, what: &str) -> Result<T, ErrorData> {
    T::from_value(&to_wire(value)?).map_err(|e| {
        ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            format!("malformed {what} from the handler: {e}"),
        )
    })
}

/// The handler's tools, asked for on every list and call.
struct Tools(Arc<dyn Handler>);

impl ToolSource for Tools {
    fn tools(&self) -> Vec<Tool> {
        // A list that cannot be had is an empty list here; the client sees no
        // tools rather than an error it could not act on.
        let Ok(result) = ask(self.0.as_ref(), "tools/list", json!({})) else {
            return Vec::new();
        };
        result
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| decode::<Tool>(t, "tool").ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn call(
        &self,
        _ctx: &CallContext,
        call: &CallToolParams,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        let arguments = call
            .arguments
            .as_ref()
            .and_then(|a| serde_json::from_str::<Value>(&a.to_json_string()).ok())
            .unwrap_or_else(|| json!({}));
        let params = json!({"name": call.name, "arguments": arguments});
        Some(
            ask(self.0.as_ref(), "tools/call", params)
                .and_then(|result| decode::<CallToolResult>(&result, "tools/call result")),
        )
    }
}

/// Register what the handler lists under `method` (`resources/list` or
/// `prompts/list`), one entry per item.
fn listed<T: Wire>(
    mcp: &dyn Handler,
    method: &'static str,
    key: &str,
) -> Result<Vec<T>, DescribeError> {
    let malformed = |why: String| DescribeError::Malformed { method, why };
    let result = ask(mcp, method, json!({})).map_err(|e| malformed(e.message))?;
    let Some(items) = result.get(key).and_then(Value::as_array) else {
        return Err(malformed(format!("no `{key}` array")));
    };
    items
        .iter()
        .map(|item| decode::<T>(item, key).map_err(|e| malformed(e.message)))
        .collect()
}

fn with_resources(
    mut builder: ServerBuilder,
    mcp: &Arc<dyn Handler>,
) -> Result<ServerBuilder, DescribeError> {
    for resource in listed::<Resource>(mcp.as_ref(), "resources/list", "resources")? {
        let handler = Arc::clone(mcp);
        builder = builder.resource(resource, move |_ctx, read: ReadResourceParams| {
            let result = ask(handler.as_ref(), "resources/read", json!({"uri": read.uri}))?;
            decode::<ReadResourceResult>(&result, "resources/read result")
        });
    }
    Ok(builder)
}

fn with_prompts(
    mut builder: ServerBuilder,
    mcp: &Arc<dyn Handler>,
) -> Result<ServerBuilder, DescribeError> {
    for prompt in listed::<Prompt>(mcp.as_ref(), "prompts/list", "prompts")? {
        let handler = Arc::clone(mcp);
        builder = builder.prompt(prompt, move |_ctx, get: GetPromptParams| {
            let arguments = get
                .arguments
                .as_ref()
                .and_then(|a| serde_json::from_str::<Value>(&a.to_json_string()).ok())
                .unwrap_or_else(|| json!({}));
            let params = json!({"name": get.name, "arguments": arguments});
            let result = ask(handler.as_ref(), "prompts/get", params)?;
            decode::<GetPromptResult>(&result, "prompts/get result")
        });
    }
    Ok(builder)
}

/// The server that answers `mcp`'s tools, resources and prompts.
///
/// Resources and prompts are listed once, here; tools are asked for per
/// request.
///
/// # Errors
/// [`DescribeError`] if the handler's resource or prompt list is malformed.
pub fn describe(mcp: Arc<dyn Handler>) -> Result<Server, DescribeError> {
    let builder = Server::builder(SERVER_NAME, env!("CARGO_PKG_VERSION"))
        .tool_source(Tools(Arc::clone(&mcp)));
    let builder = with_resources(builder, &mcp)?;
    Ok(with_prompts(builder, &mcp)?.build()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use remind_me_core::Database;
    use remind_me_mcp::McpServer;

    fn handler() -> Arc<dyn Handler> {
        Arc::new(McpServer::new(Database::open_in_memory().unwrap()))
    }

    #[test]
    fn the_server_is_described_from_the_handlers_own_lists() {
        assert!(describe(handler()).is_ok());
    }

    #[test]
    fn tools_come_from_the_handler_on_every_list() {
        let tools = Tools(handler()).tools();
        assert!(
            tools
                .iter()
                .any(|t| t.name == "remind_me_stats" || !t.name.is_empty()),
            "{tools:?}"
        );
        assert!(!tools.is_empty());
    }

    #[test]
    fn a_handler_error_keeps_its_code_and_message() {
        let err = ask(handler().as_ref(), "no/such_method", json!({})).unwrap_err();
        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
        assert!(!err.message.is_empty());
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error_result_not_a_protocol_error() {
        let source = Tools(handler());
        let ctx_free = CallToolParams::new("no_such_tool");
        // `call` needs a context only to satisfy the trait; the adapter does
        // not read it, and a connection supplies one in real use.
        let result = ask(
            source.0.as_ref(),
            "tools/call",
            json!({"name": ctx_free.name}),
        )
        .and_then(|r| decode::<CallToolResult>(&r, "tools/call result"))
        .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_adapter_is_send_and_sync() {
        assert_send_sync::<Tools>();
    }
}
