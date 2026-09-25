//! Remote MCP connector configuration and token resolution — the FT-05
//! (secret-path/bearer) slice of `#57`, plus the FT-07 OAuth *state*
//! surface (`#86`): [`OAuthStateStore`] and the issuer/state-file fields on
//! [`RemoteConfig`]/[`RemoteStatus`].
//!
//! The Streamable HTTP transport itself, and the actual OAuth authorization
//! server (PKCE verification, token issuance, the `/consent` flow), live in
//! the separate `remind_me_remote` crate (tokio/axum/rmcp), not here. This
//! module stays in `remind_me_core` deliberately: `remind_me_mcp`'s
//! `remind_me_server_status` tool needs to report whether the connector is
//! configured, and its `remind_me_revoke_clients` tool needs to list/revoke
//! OAuth clients — both from a crate that must not pull tokio/axum/rmcp in
//! just to read an env var, stat a file, or edit a small JSON document.
//! Splitting "state a sync caller can read and mutate" from "the async
//! server that serves it" mirrors [`crate::webhook::WebhookStatus`] and
//! [`crate::sync::worker::SyncWorkerStatus`], which draw the same line
//! between their status structs (here, sync, dependency-light) and their
//! actual listener threads (also here, but could just as easily not be).
//! [`OAuthStateStore`] is the same idea applied to OAuth: the reference's
//! `remind_me_revoke_clients` tool (`tools/admin.py`) operates on
//! `OAuthStateStore` directly, in the synchronous stdio process, and the
//! *running* remote server re-reads the same file on every token check —
//! that cross-process-without-an-API design is what this split preserves.
//!
//! # Token generation
//!
//! This workspace has no vetted `rand`-style CSPRNG dependency anywhere —
//! `remind_me_api`'s own `resolve_api_key` doc explicitly declined to
//! invent one for a case where an unauthenticated fallback exists. That
//! fallback doesn't exist here: the token doubles as the connector URL's
//! secret path segment (`/mcp/<token>`), so an unconfigured connector must
//! still get a real credential, not silently no-op. Rather than adding a
//! new dependency for one call site, this reuses the CSPRNG already
//! compiled into the workspace via `uuid`'s `v4` feature (backed by
//! `getrandom`, an OS-entropy source — not a hand-rolled RNG). Two
//! concatenated v4 UUIDs give roughly 244 bits of entropy, comfortably
//! above the reference's own `secrets.token_urlsafe(32)` (256 bits).
//! [`generate_token`] is `pub` (rather than private, as it was pre-`#86`) so
//! `remind_me_remote`'s OAuth module can mint auth codes, access tokens,
//! refresh tokens, and consent transaction ids the same way, instead of
//! duplicating the reasoning above next to a second copy of the same code.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Set to `1`/`true`/`yes` (or pass `--serve-remote` at the CLI layer) to
/// expose the MCP server as a remote connector. Default off — matches the
/// reference's `REMIND_ME_REMOTE_MCP`.
pub const REMOTE_ENABLED_ENV: &str = "REMIND_ME_REMOTE_MCP";
pub const REMOTE_HOST_ENV: &str = "REMIND_ME_REMOTE_HOST";
pub const REMOTE_PORT_ENV: &str = "REMIND_ME_REMOTE_PORT";
/// Connector token. Always wins over the persisted file when set.
pub const REMOTE_TOKEN_ENV: &str = "REMIND_ME_REMOTE_TOKEN";
/// Overrides where the auto-generated token is persisted. Mainly for tests —
/// see [`default_token_file`] for the real default.
pub const REMOTE_TOKEN_FILE_ENV: &str = "REMIND_ME_REMOTE_TOKEN_FILE";
/// Public HTTPS origin the OAuth authorization server (FT-07) advertises
/// itself as. Unset (the default) means OAuth is off and the connector
/// stays FT-05 secret-path/bearer only. Matches the reference's
/// `REMIND_ME_REMOTE_ISSUER` — never derive this from the `Host` header
/// (see `remind_me_remote::oauth::issuer`'s module doc for why).
pub const REMOTE_ISSUER_ENV: &str = "REMIND_ME_REMOTE_ISSUER";
/// Overrides where the OAuth client/token state file is persisted. Mainly
/// for tests — see [`default_oauth_state_file`] for the real default.
pub const REMOTE_OAUTH_STATE_FILE_ENV: &str = "REMIND_ME_REMOTE_OAUTH_STATE_FILE";

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8768;

/// Where to bind the remote MCP connector. Loopback by default — widening
/// this without a tunnel in front is the caller's job to warn about (see
/// `remind_me_remote::server::warn_if_widened`, which this type has no
/// reason to know about).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub host: String,
    pub port: u16,
    /// `REMIND_ME_REMOTE_ISSUER`, trimmed and blank-filtered. `Some` turns
    /// OAuth mode (FT-07) on in `remind_me_remote::server::build_router`.
    pub issuer: Option<String>,
}

impl RemoteConfig {
    /// Reads env vars at call time (not cached) so tests can set/unset them
    /// per case, matching this workspace's existing config conventions
    /// (e.g. `webhook::WebhookConfig::from_env`).
    pub fn from_env() -> Self {
        let host = std::env::var(REMOTE_HOST_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port = std::env::var(REMOTE_PORT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let issuer = std::env::var(REMOTE_ISSUER_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        Self { host, port, issuer }
    }
}

/// Whether `REMIND_ME_REMOTE_MCP` (or the CLI's `--serve-remote`, which sets
/// it before this is read) asked for the connector to run.
pub fn remote_enabled() -> bool {
    std::env::var(REMOTE_ENABLED_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Default token file location: `~/.remind-me/connector_token`, alongside
/// the database ([`crate::db::DEFAULT_DIR_NAME`]) — falling back to
/// `~/.remind_me/connector_token` (underscore) if that's the only one that
/// exists, via [`crate::db::resolve_memory_dir_child`] (#228). No caller
/// relying on this default (no [`REMOTE_TOKEN_FILE_ENV`]/`REMIND_ME_MCP_DIR`
/// override) silently drifts from wherever the real connector's token
/// already lives, whichever of the two directories that turns out to be.
fn default_token_file() -> PathBuf {
    crate::db::resolve_memory_dir_child("connector_token")
}

/// The effective token file path: [`REMOTE_TOKEN_FILE_ENV`] if set and
/// non-blank, otherwise [`default_token_file`].
pub fn token_file_path() -> PathBuf {
    std::env::var(REMOTE_TOKEN_FILE_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(default_token_file)
}

/// Default OAuth state file location: `~/.remind-me/oauth.json`, sibling of
/// [`default_token_file`] — same fallback, same reason.
fn default_oauth_state_file() -> PathBuf {
    crate::db::resolve_memory_dir_child("oauth.json")
}

/// The effective OAuth state file path: [`REMOTE_OAUTH_STATE_FILE_ENV`] if
/// set and non-blank, otherwise [`default_oauth_state_file`].
pub fn oauth_state_file_path() -> PathBuf {
    std::env::var(REMOTE_OAUTH_STATE_FILE_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(default_oauth_state_file)
}

/// Mint a high-entropy token (see this module's doc for why two UUIDs
/// rather than a new CSPRNG dependency). Used for the connector token
/// itself and, by `remind_me_remote`'s OAuth module, for auth codes, access
/// tokens, refresh tokens, and consent transaction ids.
pub fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Resolve the effective connector token (SE-01 parity with the reference's
/// `resolve_connector_token`, and this port's own `resolve_api_key`
/// precedent):
///
/// 1. [`REMOTE_TOKEN_ENV`] — always wins when set and non-blank.
/// 2. The token persisted at [`token_file_path`].
/// 3. First use: generate one, persist it (`0600` on unix), and return it.
///
/// A token file that can be neither read nor written yields an ephemeral
/// per-process token instead of ever leaving the endpoint unauthenticated.
pub fn resolve_connector_token() -> String {
    if let Ok(env_token) = std::env::var(REMOTE_TOKEN_ENV) {
        let env_token = env_token.trim().to_string();
        if !env_token.is_empty() {
            return env_token;
        }
    }

    let file = token_file_path();
    if let Ok(existing) = fs::read_to_string(&file) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }

    let token = generate_token();
    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&file, format!("{token}\n")).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o600));
        }
    }
    token
}

/// What `remind_me_server_status` reports for the remote MCP connector.
///
/// Mirrors [`crate::webhook::WebhookStatus`] / [`crate::sync::worker::SyncWorkerStatus`]'s
/// shape: plain, `Serialize`-able, gathered by a sync free function. Matches
/// the reference's `get_remote_status()` field-for-field, including the
/// FT-07 OAuth fields (`oauth_enabled`, `issuer`, `oauth_state_file`,
/// `oauth_clients`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub token_file: String,
    pub token_configured: bool,
    pub oauth_enabled: bool,
    pub issuer: Option<String>,
    pub oauth_state_file: String,
    pub oauth_clients: usize,
}

/// Gather [`RemoteStatus`]. Reads env vars and stats the token/state files
/// at call time (not cached) so tests can monkeypatch them, same as
/// [`RemoteConfig::from_env`].
pub fn remote_status() -> RemoteStatus {
    let config = RemoteConfig::from_env();
    let token_file = token_file_path();
    let token_configured = std::env::var(REMOTE_TOKEN_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || token_file.is_file();
    let oauth_state_file = oauth_state_file_path();
    let oauth_clients = OAuthStateStore::new(oauth_state_file.clone())
        .list_clients()
        .len();
    RemoteStatus {
        enabled: remote_enabled(),
        host: config.host,
        port: config.port,
        token_file: token_file.to_string_lossy().to_string(),
        token_configured,
        oauth_enabled: config.issuer.is_some(),
        issuer: config.issuer,
        oauth_state_file: oauth_state_file.to_string_lossy().to_string(),
        oauth_clients,
    }
}

// ---------------------------------------------------------------------------
// OAuth state (FT-07, `#86`) — clients + token hashes, shared with the
// running remote server via the same JSON file.
// ---------------------------------------------------------------------------

/// Which token bucket a stored/looked-up token hash belongs to. A typed
/// alternative to the reference's stringly-typed `kind` parameter
/// (`"access_tokens"` / `"refresh_tokens"`) — same two buckets, no risk of a
/// typo'd literal silently missing every lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Refresh,
}

/// Counts of tokens a bulk operation touched, keyed the same way the
/// reference's `_drop_tokens` return dict is (`access_tokens`/`refresh_tokens`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenCounts {
    pub access_tokens: usize,
    pub refresh_tokens: usize,
}

/// Summary of one registered client for `remind_me_revoke_clients`' listing
/// — mirrors the reference's `OAuthStateStore.list_clients()` dict shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthClientSummary {
    pub client_id: String,
    pub client_name: Option<String>,
    pub client_id_issued_at: Option<i64>,
    pub redirect_uris: Option<Vec<String>>,
    pub access_tokens: usize,
    pub refresh_tokens: usize,
}

/// What revoking a client reports — the reference's `revoke_client` return
/// dict (`client_id`, `client_name`, plus the counts of tokens dropped).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthRevokeSummary {
    pub client_id: String,
    pub client_name: Option<String>,
    pub access_tokens: usize,
    pub refresh_tokens: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OAuthState {
    #[serde(default)]
    clients: Map<String, Value>,
    #[serde(default)]
    access_tokens: Map<String, Value>,
    #[serde(default)]
    refresh_tokens: Map<String, Value>,
}

impl OAuthState {
    fn tokens(&self, kind: TokenKind) -> &Map<String, Value> {
        match kind {
            TokenKind::Access => &self.access_tokens,
            TokenKind::Refresh => &self.refresh_tokens,
        }
    }

    fn tokens_mut(&mut self, kind: TokenKind) -> &mut Map<String, Value> {
        match kind {
            TokenKind::Access => &mut self.access_tokens,
            TokenKind::Refresh => &mut self.refresh_tokens,
        }
    }

    /// Remove every token (both buckets) belonging to `client_id`, mutating
    /// `self`. Mirrors the reference's `OAuthStateStore._drop_tokens`.
    fn drop_tokens(&mut self, client_id: &str) -> OAuthTokenCounts {
        let belongs =
            |meta: &Value| meta.get("client_id").and_then(Value::as_str) == Some(client_id);
        let access_doomed: Vec<String> = self
            .access_tokens
            .iter()
            .filter(|(_, meta)| belongs(meta))
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in &access_doomed {
            self.access_tokens.remove(hash);
        }
        let refresh_doomed: Vec<String> = self
            .refresh_tokens
            .iter()
            .filter(|(_, meta)| belongs(meta))
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in &refresh_doomed {
            self.refresh_tokens.remove(hash);
        }
        OAuthTokenCounts {
            access_tokens: access_doomed.len(),
            refresh_tokens: refresh_doomed.len(),
        }
    }
}

/// SHA-256 hex digest of a token — what the state file stores instead of
/// the raw secret (mirrors the reference's `_hash_token`).
fn hash_token(token: &str) -> String {
    sha256::digest(token)
}

/// JSON-file persistence for OAuth state (FT-07, `#86`).
///
/// Layout: `{"clients": {client_id: <RFC 7591 record>}, "access_tokens":
/// {sha256_hex: {client_id, scopes, expires_at, resource}}, "refresh_tokens":
/// {sha256_hex: {client_id, scopes, expires_at}}}` — a direct port of the
/// reference's `OAuthStateStore` layout, so the state file this crate writes
/// and the one the reference writes are structurally interchangeable. The
/// file is created with `0600` permissions (SE-01 conventions, matching
/// `resolve_connector_token`'s own token file) and re-read on every
/// operation, so a mutation from another process (the remote server
/// checking a bearer token, or `remind_me_revoke_clients` in the stdio
/// process) is visible immediately, with no cache to invalidate. The
/// in-process [`Mutex`] only serializes this process's own read-modify-write
/// cycles; cross-process locking is out of scope for a single-user store,
/// same as the reference.
///
/// Client and token-metadata records are stored as raw [`Value`] rather than
/// a fixed struct, deliberately: this crate has no reason to know the shape
/// of an RFC 7591 client record or a token's `scopes`/`resource` fields —
/// only `remind_me_remote`'s OAuth provider does. Keeping the store
/// schema-agnostic here is what lets `remind_me_core` (and thus
/// `remind_me_mcp`'s sync `remind_me_revoke_clients` tool) read/revoke
/// clients without depending on `remind_me_remote` at all.
pub struct OAuthStateStore {
    path: PathBuf,
    lock: Mutex<()>,
    /// Distinguishes this store's temp files from any other process's when
    /// several share a directory. See [`Self::temp_path`].
    temp_counter: std::sync::atomic::AtomicU64,
}

impl OAuthStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
            temp_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_once(&self) -> Option<OAuthState> {
        let raw = fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Load state from disk, tolerating a missing or corrupt file (reads
    /// as empty rather than propagating an error — same tolerance as the
    /// reference's `_read`).
    ///
    /// This used to retry a failed read up to eight times with a backoff,
    /// because a file this very process had just written and verified could
    /// transiently read back as missing under parallel test load. That was
    /// never a filesystem quirk to be waited out: [`Self::write`] truncated
    /// the real path in place, so a concurrent reader genuinely could catch
    /// it empty or half-written. Writing to a temp file and renaming makes
    /// that window not exist — a reader sees the old complete file or the
    /// new one — so the retries have nothing left to paper over and are
    /// gone with the cause (issue #160).
    fn read(&self) -> OAuthState {
        self.read_once().unwrap_or_default()
    }

    /// A sibling temp path in the same directory as the state file.
    ///
    /// Same directory, not the system temp dir: `rename` is only atomic
    /// within one filesystem, and `/tmp` is frequently a different one.
    /// The name carries the pid and a per-store counter so two processes
    /// (or two stores in one process) writing concurrently cannot collide
    /// on it and corrupt each other's in-progress write.
    fn temp_path(&self) -> PathBuf {
        let n = self
            .temp_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stem = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "oauth-state.json".to_string());
        let name = format!(".{stem}.{}.{n}.tmp", std::process::id());
        match self.path.parent() {
            Some(parent) => parent.join(name),
            None => PathBuf::from(name),
        }
    }

    /// Persist `state` atomically, creating the parent directory if needed
    /// and setting `0600` permissions on unix.
    ///
    /// Writes a sibling temp file, fsyncs it, then renames it over the real
    /// path. `rename` is atomic, so a concurrent reader observes either the
    /// complete old file or the complete new one and never a truncated one
    /// (issue #160). The previous implementation truncated the real path in
    /// place, which is what made a reader's "the file is missing" possible
    /// at all, and what the retry-and-verify loop here was really working
    /// around.
    ///
    /// Permissions are set on the temp file *before* the rename, so the
    /// state file never exists at its real path with default permissions,
    /// not even briefly. The old order — write, then chmod — left exactly
    /// that window on every single write.
    ///
    /// **Failures propagate.** They used to be swallowed after a few
    /// retries, on the reasoning that a caller holding the token in memory
    /// keeps working. That is the wrong trade for the one caller that
    /// matters: `issue_tokens` would hand a client a bearer token that was
    /// never persisted, so the very next authenticated request fails with
    /// no indication why, on either side. A refused issuance is a worse
    /// user experience and a far better failure.
    fn write(&self, state: &OAuthState) -> io::Result<()> {
        let body = serde_json::to_string_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let body = format!("{body}\n");

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp = self.temp_path();
        // Scoped so the handle is closed before the rename: Windows refuses
        // to rename over a path while a handle to the source is open.
        let written = (|| -> io::Result<()> {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }

        if let Err(e) = fs::rename(&tmp, &self.path) {
            // Leaving a stray temp file behind would be a slow leak in the
            // directory the state file lives in.
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }

    // -- clients --------------------------------------------------------

    /// Return the stored registration record for `client_id`, or `None`.
    pub fn get_client(&self, client_id: &str) -> Option<Value> {
        self.read().clients.get(client_id).cloned()
    }

    /// Insert or replace a client registration record.
    ///
    /// Returns the write error rather than swallowing it: a registration
    /// the caller believes succeeded but that never reached disk means the
    /// client is authenticated for this process's lifetime and a stranger
    /// after the next restart (issue #160).
    pub fn put_client(&self, client_id: &str, record: Value) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        state.clients.insert(client_id.to_string(), record);
        self.write(&state)
    }

    /// Summarise registered clients with live token counts (for
    /// `remind_me_revoke_clients`).
    pub fn list_clients(&self) -> Vec<OAuthClientSummary> {
        let state = self.read();
        state
            .clients
            .iter()
            .map(|(client_id, record)| {
                let count = |kind: TokenKind| {
                    state
                        .tokens(kind)
                        .values()
                        .filter(|meta| {
                            meta.get("client_id").and_then(Value::as_str) == Some(client_id)
                        })
                        .count()
                };
                OAuthClientSummary {
                    client_id: client_id.clone(),
                    client_name: record
                        .get("client_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    client_id_issued_at: record.get("client_id_issued_at").and_then(Value::as_i64),
                    redirect_uris: record.get("redirect_uris").and_then(Value::as_array).map(
                        |arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        },
                    ),
                    access_tokens: count(TokenKind::Access),
                    refresh_tokens: count(TokenKind::Refresh),
                }
            })
            .collect()
    }

    /// Delete a client registration and every token it holds.
    ///
    /// Returns `None` when `client_id` is unknown — this is the
    /// "revoke one, by id" operation; there is no "revoke all" (see this
    /// module's tests and `remind_me_mcp`'s `remind_me_revoke_clients` tool
    /// doc for why an empty `client_id` there means *list*, not *revoke
    /// everything*).
    /// `Ok(None)` when the client was not registered; `Err` when the
    /// revocation could not be persisted — which must not read as success,
    /// since the client would still be authenticated after a restart
    /// (issue #160).
    pub fn revoke_client(&self, client_id: &str) -> io::Result<Option<OAuthRevokeSummary>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        let Some(record) = state.clients.remove(client_id) else {
            return Ok(None);
        };
        let counts = state.drop_tokens(client_id);
        self.write(&state)?;
        Ok(Some(OAuthRevokeSummary {
            client_id: client_id.to_string(),
            client_name: record
                .get("client_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            access_tokens: counts.access_tokens,
            refresh_tokens: counts.refresh_tokens,
        }))
    }

    // -- tokens -----------------------------------------------------------

    /// Store `token`'s hash (never the raw secret) under `kind`.
    ///
    /// The failure that matters most (issue #160): a token handed to a
    /// client but never persisted is rejected on its first use, with
    /// nothing on either side explaining why.
    pub fn put_token(&self, kind: TokenKind, token: &str, meta: Value) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        state.tokens_mut(kind).insert(hash_token(token), meta);
        self.write(&state)
    }

    /// Look up a raw token by hash; `None` when unknown.
    pub fn get_token(&self, kind: TokenKind, token: &str) -> Option<Value> {
        self.read().tokens(kind).get(&hash_token(token)).cloned()
    }

    /// Forget a raw token (no-op when unknown).
    ///
    /// A silently-failed delete is a revocation that did not happen, so
    /// this reports rather than swallows (issue #160).
    pub fn delete_token(&self, kind: TokenKind, token: &str) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        let hash = hash_token(token);
        if state.tokens_mut(kind).remove(&hash).is_some() {
            return self.write(&state);
        }
        Ok(())
    }

    /// Drop every access/refresh token of `client_id`, keeping the
    /// registration — what RFC 7009 revocation of any one token escalates
    /// to (the reference's `revoke_token`).
    pub fn delete_tokens_for_client(&self, client_id: &str) -> io::Result<OAuthTokenCounts> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.read();
        let counts = state.drop_tokens(client_id);
        if counts.access_tokens > 0 || counts.refresh_tokens > 0 {
            self.write(&state)?;
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env vars are process-global; serialize this module's tests so they
    // don't race each other the way `cargo test` otherwise would.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        crate::test_env::remove_var(REMOTE_ENABLED_ENV);
        crate::test_env::remove_var(REMOTE_HOST_ENV);
        crate::test_env::remove_var(REMOTE_PORT_ENV);
        crate::test_env::remove_var(REMOTE_TOKEN_ENV);
        crate::test_env::remove_var(REMOTE_TOKEN_FILE_ENV);
        crate::test_env::remove_var(REMOTE_ISSUER_ENV);
        crate::test_env::remove_var(REMOTE_OAUTH_STATE_FILE_ENV);
    }

    #[test]
    fn remote_config_defaults_to_loopback_and_the_reference_port() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let config = RemoteConfig::from_env();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8768);
        clear_env();
    }

    #[test]
    fn remote_config_reads_host_and_port_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(REMOTE_HOST_ENV, "0.0.0.0");
        crate::test_env::set_var(REMOTE_PORT_ENV, "9999");
        let config = RemoteConfig::from_env();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 9999);
        clear_env();
    }

    #[test]
    fn remote_enabled_is_false_by_default_and_true_for_recognised_truthy_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(!remote_enabled());
        for value in ["1", "true", "TRUE", "yes"] {
            crate::test_env::set_var(REMOTE_ENABLED_ENV, value);
            assert!(remote_enabled(), "{value:?} should enable the connector");
        }
        crate::test_env::set_var(REMOTE_ENABLED_ENV, "0");
        assert!(!remote_enabled());
        clear_env();
    }

    #[test]
    fn default_token_and_oauth_paths_share_the_hyphenated_data_directory() {
        // Regression: these used to hardcode `.remind_me` (underscored), a
        // directory nothing else in this port reads or writes -- `configure`
        // and the database resolver both settled on `.remind-me` (hyphenated)
        // long ago. A default that disagreed meant a caller with no
        // REMOTE_TOKEN_FILE_ENV/REMOTE_OAUTH_STATE_FILE_ENV override (e.g.
        // `remind_me_revoke_clients` run from a process that isn't the
        // configured connector) silently looked in the wrong place and
        // always found an empty client list.
        let token = default_token_file();
        let oauth = default_oauth_state_file();
        assert_eq!(token.file_name().unwrap(), "connector_token");
        assert_eq!(oauth.file_name().unwrap(), "oauth.json");
        assert_eq!(
            token.parent().unwrap().file_name().unwrap(),
            crate::db::DEFAULT_DIR_NAME
        );
        assert_eq!(
            oauth.parent().unwrap().file_name().unwrap(),
            crate::db::DEFAULT_DIR_NAME
        );
    }

    #[test]
    fn resolve_connector_token_prefers_the_env_var_over_a_persisted_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("rrm_remote_token_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("connector_token");
        fs::write(&file, "file-token\n").unwrap();
        crate::test_env::set_var(REMOTE_TOKEN_FILE_ENV, &file);
        crate::test_env::set_var(REMOTE_TOKEN_ENV, "env-token");

        assert_eq!(resolve_connector_token(), "env-token");

        crate::test_env::remove_var(REMOTE_TOKEN_ENV);
        assert_eq!(resolve_connector_token(), "file-token");

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    #[test]
    fn resolve_connector_token_generates_and_persists_on_first_use() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("rrm_remote_token_gen_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("nested").join("connector_token");
        crate::test_env::set_var(REMOTE_TOKEN_FILE_ENV, &file);

        assert!(!file.is_file());
        let first = resolve_connector_token();
        assert!(!first.is_empty());
        assert!(file.is_file());

        // Second resolution reuses the persisted token rather than
        // generating a fresh one every call.
        let second = resolve_connector_token();
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    #[test]
    fn remote_status_reports_token_configured_once_a_token_exists_on_disk() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir = std::env::temp_dir().join(format!("rrm_remote_status_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("connector_token");
        crate::test_env::set_var(REMOTE_TOKEN_FILE_ENV, &file);
        crate::test_env::set_var(REMOTE_OAUTH_STATE_FILE_ENV, dir.join("oauth.json"));

        let status = remote_status();
        assert!(!status.enabled);
        assert!(!status.token_configured);
        assert!(!status.oauth_enabled);
        assert!(status.issuer.is_none());

        fs::write(&file, "a-token\n").unwrap();
        let status = remote_status();
        assert!(status.token_configured);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    #[test]
    fn remote_config_reads_the_issuer_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(RemoteConfig::from_env().issuer, None);

        crate::test_env::set_var(REMOTE_ISSUER_ENV, "  https://machine.tailnet.ts.net  ");
        assert_eq!(
            RemoteConfig::from_env().issuer,
            Some("https://machine.tailnet.ts.net".to_string())
        );

        // Blank (whitespace-only) is treated the same as unset.
        crate::test_env::set_var(REMOTE_ISSUER_ENV, "   ");
        assert_eq!(RemoteConfig::from_env().issuer, None);
        clear_env();
    }

    #[test]
    fn remote_status_reports_oauth_enabled_and_the_client_count_once_an_issuer_and_clients_exist() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let dir =
            std::env::temp_dir().join(format!("rrm_remote_status_oauth_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("oauth.json");
        crate::test_env::set_var(REMOTE_TOKEN_FILE_ENV, dir.join("connector_token"));
        crate::test_env::set_var(REMOTE_OAUTH_STATE_FILE_ENV, &state_file);
        crate::test_env::set_var(REMOTE_ISSUER_ENV, "https://machine.tailnet.ts.net");

        let status = remote_status();
        assert!(status.oauth_enabled);
        assert_eq!(
            status.issuer.as_deref(),
            Some("https://machine.tailnet.ts.net")
        );
        assert_eq!(status.oauth_state_file, state_file.to_string_lossy());
        assert_eq!(status.oauth_clients, 0);

        OAuthStateStore::new(&state_file)
            .put_client("client-1", serde_json::json!({"client_name": "claude.ai"}))
            .expect("write");
        assert_eq!(remote_status().oauth_clients, 1);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }

    // -- OAuthStateStore --------------------------------------------------

    fn temp_state_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rrm_oauth_store_{label}_{}_{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        dir.join("oauth.json")
    }

    #[test]
    fn oauth_store_tolerates_a_missing_or_corrupt_state_file() {
        let path = temp_state_path("missing");
        let store = OAuthStateStore::new(&path);
        assert_eq!(store.list_clients(), Vec::new());
        assert!(store.get_client("x").is_none());
        assert!(store.revoke_client("x").unwrap().is_none());

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert_eq!(store.list_clients(), Vec::new());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_round_trips_a_client_registration() {
        let path = temp_state_path("client");
        let store = OAuthStateStore::new(&path);
        let record = serde_json::json!({
            "client_name": "claude.ai",
            "client_id_issued_at": 1_700_000_000i64,
            "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"],
        });
        store.put_client("client-1", record.clone()).expect("write");

        assert_eq!(store.get_client("client-1"), Some(record));
        let clients = store.list_clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].client_id, "client-1");
        assert_eq!(clients[0].client_name.as_deref(), Some("claude.ai"));
        assert_eq!(clients[0].access_tokens, 0);
        assert_eq!(clients[0].refresh_tokens, 0);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_state_file_has_0600_permissions() {
        let path = temp_state_path("perms");
        let store = OAuthStateStore::new(&path);
        store
            .put_client("c1", serde_json::json!({}))
            .expect("write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_never_persists_raw_token_values() {
        let path = temp_state_path("hash");
        let store = OAuthStateStore::new(&path);
        store
            .put_token(
                TokenKind::Access,
                "super-secret-raw-access-token",
                serde_json::json!({ "client_id": "c1" }),
            )
            .expect("write");

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("super-secret-raw-access-token"));
        assert!(store
            .get_token(TokenKind::Access, "super-secret-raw-access-token")
            .is_some());
        assert!(store.get_token(TokenKind::Access, "wrong-token").is_none());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_delete_tokens_for_client_drops_only_the_targeted_clients_tokens() {
        let path = temp_state_path("delete");
        let store = OAuthStateStore::new(&path);
        store
            .put_token(
                TokenKind::Access,
                "tok-a",
                serde_json::json!({ "client_id": "c1" }),
            )
            .expect("write");
        store
            .put_token(
                TokenKind::Refresh,
                "tok-r",
                serde_json::json!({ "client_id": "c1" }),
            )
            .expect("write");
        store
            .put_token(
                TokenKind::Access,
                "tok-b",
                serde_json::json!({ "client_id": "c2" }),
            )
            .expect("write");

        let counts = store.delete_tokens_for_client("c1").expect("write");
        assert_eq!(counts.access_tokens, 1);
        assert_eq!(counts.refresh_tokens, 1);
        assert!(store.get_token(TokenKind::Access, "tok-a").is_none());
        assert_eq!(
            store.get_token(TokenKind::Access, "tok-b"),
            Some(serde_json::json!({ "client_id": "c2" }))
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn oauth_store_revoke_client_deletes_the_registration_and_its_tokens_but_leaves_other_clients_alone(
    ) {
        let path = temp_state_path("revoke");
        let store = OAuthStateStore::new(&path);
        store
            .put_client("c1", serde_json::json!({ "client_name": "claude.ai" }))
            .expect("write");
        store
            .put_client("c2", serde_json::json!({ "client_name": "other" }))
            .expect("write");
        store
            .put_token(
                TokenKind::Access,
                "tok-a",
                serde_json::json!({ "client_id": "c1" }),
            )
            .expect("write");
        store
            .put_token(
                TokenKind::Refresh,
                "tok-r",
                serde_json::json!({ "client_id": "c1" }),
            )
            .expect("write");

        let summary = store
            .revoke_client("c1")
            .expect("write")
            .expect("c1 is registered");
        assert_eq!(summary.client_id, "c1");
        assert_eq!(summary.client_name.as_deref(), Some("claude.ai"));
        assert_eq!(summary.access_tokens, 1);
        assert_eq!(summary.refresh_tokens, 1);

        assert!(store.get_client("c1").is_none());
        assert!(store.get_token(TokenKind::Access, "tok-a").is_none());
        let remaining: Vec<String> = store
            .list_clients()
            .into_iter()
            .map(|c| c.client_id)
            .collect();
        assert_eq!(remaining, vec!["c2".to_string()]);

        // Revoking an already-unknown client_id is a no-op that reports None
        // -- not an error, not a silent "revoked everything".
        assert!(store.revoke_client("c1").unwrap().is_none());
        assert!(store.revoke_client("no-such-client").unwrap().is_none());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    // -- OAuth state durability (issue #160) --------------------------------

    use serde_json::json;
    use std::path::PathBuf;

    /// A path whose parent is a regular file, so every write fails.
    ///
    /// Chosen over a read-only directory because these tests also run as
    /// root in CI containers, where mode bits are simply bypassed and a
    /// permission-based injection quietly succeeds instead of failing.
    fn unwritable_store() -> (OAuthStateStore, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "rrm_oauth_unwritable_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::write(&base, b"not a directory").expect("seed blocker file");
        let store = OAuthStateStore::new(base.join("nested").join("oauth.json"));
        (store, base)
    }

    #[test]
    fn a_write_that_cannot_land_is_reported_not_swallowed() {
        let (store, base) = unwritable_store();

        // Every mutator must surface it. Previously all of these returned
        // (), so a token could be handed to a client that was never stored.
        assert!(store
            .put_token(TokenKind::Access, "tok", json!({}))
            .is_err());
        assert!(store.put_client("cid", json!({})).is_err());

        let _ = fs::remove_file(&base);
    }

    #[test]
    fn a_successful_write_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!("rrm_oauth_tmp_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = OAuthStateStore::new(dir.join("oauth.json"));
        store
            .put_token(TokenKind::Access, "tok", json!({"a": 1}))
            .unwrap();

        let strays: Vec<_> = fs::read_dir(&dir)
            .expect("state dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "oauth.json")
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file_behind() {
        // The rename is what can fail after the temp file exists, so the
        // cleanup path needs its own check -- otherwise a store on a
        // failing volume slowly fills its directory with debris.
        let dir = std::env::temp_dir().join(format!("rrm_oauth_tmpfail_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        // Make the destination a directory: the write succeeds, the rename
        // onto a non-empty directory does not.
        let target = dir.join("oauth.json");
        fs::create_dir_all(target.join("occupied")).expect("blocker dir");

        let store = OAuthStateStore::new(&target);
        assert!(store
            .put_token(TokenKind::Access, "tok", json!({}))
            .is_err());

        let strays: Vec<_> = fs::read_dir(&dir)
            .expect("state dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "oauth.json")
            .collect();
        assert!(
            strays.is_empty(),
            "temp files left behind after failure: {strays:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_state_file_is_never_world_readable_even_briefly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rrm_oauth_perm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = OAuthStateStore::new(dir.join("oauth.json"));
        store
            .put_token(TokenKind::Access, "tok", json!({}))
            .unwrap();

        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file mode is {mode:o}, expected 600");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The failure this whole issue is about: a reader must never observe a
    /// half-written state file.
    ///
    /// Under the old truncate-in-place write this raced -- a reader could
    /// catch the file empty and read it as "no tokens", which is what made
    /// a just-issued token vanish. With write-temp-then-rename the reader
    /// sees the old complete file or the new one, so every observation
    /// must parse and every one must contain the token written before the
    /// hammering began.
    #[test]
    fn concurrent_readers_never_observe_a_torn_write() {
        let dir = std::env::temp_dir().join(format!("rrm_oauth_torn_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = std::sync::Arc::new(OAuthStateStore::new(dir.join("oauth.json")));
        store
            .put_token(TokenKind::Access, "anchor", json!({"n": 0}))
            .unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || {
                for n in 1..=200 {
                    store
                        .put_token(TokenKind::Access, &format!("t{n}"), json!({"n": n}))
                        .expect("write");
                }
                stop.store(true, std::sync::atomic::Ordering::Release);
            })
        };

        let mut observations = 0u32;
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            // The anchor was durably written before any concurrent writing
            // started, so it is present in every valid state. Reading it as
            // absent means a torn or truncated file was observed.
            assert!(
                store.get_token(TokenKind::Access, "anchor").is_some(),
                "observed a state file without the anchor token -- torn write"
            );
            observations += 1;
        }
        writer.join().expect("writer thread");
        assert!(observations > 0, "reader never ran");

        let _ = fs::remove_dir_all(&dir);
    }
}
