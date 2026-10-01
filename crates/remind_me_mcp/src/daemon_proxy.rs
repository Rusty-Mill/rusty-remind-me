//! An MCP [`Handler`] that relays every line to the daemon's MCP server.

use crate::Handler;
use remind_me_core::daemon::client::{self, ConnectError, DaemonConnection};
use remind_me_core::daemon::endpoint::Endpoint;
use remind_me_core::daemon::wire::Mode;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

/// Relays MCP lines to the daemon over one connection.
///
/// If the connection drops (the daemon was stopped or restarted), the next
/// line reconnects once, starting a daemon if none is running, before giving
/// up with a JSON-RPC error. The new connection is a new session, so a client
/// identity from the old `initialize` is lost; the memories it writes then
/// record `REMIND_ME_CLIENT` instead, as they would before a handshake.
pub struct DaemonProxy {
    endpoint: Endpoint,
    exe: PathBuf,
    connection: Mutex<Option<DaemonConnection>>,
}

impl DaemonProxy {
    /// Connect to the daemon for `endpoint`, starting it from `exe` if needed.
    pub fn connect(endpoint: Endpoint, exe: PathBuf) -> Result<Self, ConnectError> {
        let connection = client::connect_or_start(&endpoint, &exe, Mode::Mcp)?;
        Ok(Self {
            endpoint,
            exe,
            connection: Mutex::new(Some(connection)),
        })
    }

    fn relay(&self, line: &str) -> Result<Option<Value>, String> {
        let mut slot = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(connection) = slot.as_mut() {
            match connection.call_mcp(line) {
                Ok(reply) => return Ok(reply),
                Err(e) => eprintln!("rusty-remind-me: lost the daemon ({e}); reconnecting"),
            }
        }
        *slot = None;
        let mut connection = client::connect_or_start(&self.endpoint, &self.exe, Mode::Mcp)
            .map_err(|e| format!("the store daemon is unavailable: {e}"))?;
        let reply = connection
            .call_mcp(line)
            .map_err(|e| format!("the store daemon is unavailable: {e}"))?;
        *slot = Some(connection);
        Ok(reply)
    }
}

impl Handler for DaemonProxy {
    fn handle_line(&self, line: &str) -> Option<Value> {
        match self.relay(line) {
            Ok(reply) => reply,
            // A notification has no reply to carry the error; a request does.
            Err(message) => {
                let request: Value = serde_json::from_str(line).ok()?;
                request.get("id")?;
                Some(crate::error_response(line, &message))
            }
        }
    }
}
