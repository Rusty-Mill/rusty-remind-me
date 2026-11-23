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
//! server that serves it" mirrors [`crate::webhook::WebhookStatus`], which
//! draws the same line between its status struct (here, sync,
//! dependency-light) and its actual listener thread (also here, but could
//! just as easily not be).
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
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
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
    if let Some(existing) = existing_connector_token() {
        return existing;
    }

    let file = token_file_path();
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

/// Capture-only connector credential (ADR-0036). Unset means there is
/// none. A request carrying it may only open an MCP session and call
/// `remind_me_capture_transcript`; `remind_me_remote::auth` enforces that.
/// It exists for hooks in places that cannot hold the full connector token,
/// such as a plugin uploaded to claude.ai.
pub const REMOTE_CAPTURE_TOKEN_ENV: &str = "REMIND_ME_REMOTE_CAPTURE_TOKEN";

/// Shortest capture token accepted. It is a bearer credential reachable
/// from the internet, so a short one is refused rather than trusted.
pub const MIN_CAPTURE_TOKEN_LEN: usize = 32;

/// The capture-only credential as configured. A credential that gates
/// access resolves to off, unusable or ready, never to a default.
#[derive(Clone, PartialEq, Eq)]
pub enum CaptureToken {
    /// [`REMOTE_CAPTURE_TOKEN_ENV`] is unset.
    Off,
    /// Set, but refused; capture-only access stays closed.
    Unusable { reason: &'static str },
    /// Set and usable.
    Ready(String),
}

impl std::fmt::Debug for CaptureToken {
    /// Never prints the token itself.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("Off"),
            Self::Unusable { reason } => write!(f, "Unusable({reason})"),
            Self::Ready(_) => f.write_str("Ready(<redacted>)"),
        }
    }
}

impl CaptureToken {
    /// Judge `raw` as a capture token beside the full `connector_token`.
    pub fn parse(raw: &str, connector_token: &str) -> Self {
        let token = raw.trim();
        if token.is_empty() {
            return Self::Unusable { reason: "is blank" };
        }
        if token.len() < MIN_CAPTURE_TOKEN_LEN {
            return Self::Unusable {
                reason: "is shorter than 32 characters",
            };
        }
        if !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Self::Unusable {
                reason: "has characters other than visible ASCII",
            };
        }
        if crate::webhook::constant_time_eq(token.as_bytes(), connector_token.trim().as_bytes()) {
            return Self::Unusable {
                reason: "is the full connector token",
            };
        }
        Self::Ready(token.to_string())
    }

    /// `off`, `ready` or `unusable: <reason>`, for a status report.
    pub fn label(&self) -> String {
        match self {
            Self::Off => "off".to_string(),
            Self::Unusable { reason } => format!("unusable: {REMOTE_CAPTURE_TOKEN_ENV} {reason}"),
            Self::Ready(_) => "ready".to_string(),
        }
    }

    /// The token, when it is ready.
    pub fn ready(&self) -> Option<&str> {
        match self {
            Self::Ready(token) => Some(token),
            _ => None,
        }
    }
}

/// [`REMOTE_CAPTURE_TOKEN_ENV`], judged beside `connector_token`. Unlike the
/// connector token it is never generated or persisted: off is a valid state.
pub fn resolve_capture_token(connector_token: &str) -> CaptureToken {
    match std::env::var(REMOTE_CAPTURE_TOKEN_ENV) {
        Ok(raw) => CaptureToken::parse(&raw, connector_token),
        Err(_) => CaptureToken::Off,
    }
}

/// The connector token from [`REMOTE_TOKEN_ENV`] or the token file, without
/// generating one: steps 1 and 2 of [`resolve_connector_token`].
fn existing_connector_token() -> Option<String> {
    let from_env = std::env::var(REMOTE_TOKEN_ENV)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    from_env.or_else(|| {
        fs::read_to_string(token_file_path())
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    })
}

/// What `remind_me_server_status` reports for the remote MCP connector.
///
/// Mirrors [`crate::webhook::WebhookStatus`]'s shape: plain, `Serialize`-able, gathered by a sync free function. Matches
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
    /// The capture-only credential's state (`off`, `ready` or
    /// `unusable: <reason>`), never its value.
    pub capture_token: String,
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
        capture_token: resolve_capture_token(&existing_connector_token().unwrap_or_default())
            .label(),
    }
}

// ---------------------------------------------------------------------------
// OAuth state (FT-07, `#86`) — clients + token hashes, shared with the
// running remote server via the same JSON file.
// ---------------------------------------------------------------------------

/// OAuth state persistence lives in the shared `rusty_oauth_server` crate,
/// which remind-me's remote server, rusty_tick and Mill Board all use. These
/// are its types under the names this crate has always exported, so
/// `remind_me_revoke_clients` and `remote_status` read the same file the
/// running server writes, without depending on `remind_me_remote`.
pub use rusty_oauth_server::{
    ClientSummary as OAuthClientSummary, RevokeSummary as OAuthRevokeSummary,
    StateStore as OAuthStateStore, TokenCounts as OAuthTokenCounts, TokenKind,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
        crate::test_env::remove_var(REMOTE_CAPTURE_TOKEN_ENV);
    }

    const CONNECTOR: &str = "connector-token-0123456789abcdef0123456789";
    const CAPTURE: &str = "capture-token-0123456789abcdef0123456789ab";

    #[test]
    fn a_capture_token_is_ready_only_when_long_visible_and_distinct() {
        assert_eq!(
            CaptureToken::parse(&format!("  {CAPTURE}\n"), CONNECTOR).ready(),
            Some(CAPTURE)
        );
        for (raw, reason) in [
            ("   ", "is blank"),
            ("short-token", "is shorter than 32 characters"),
            (
                "capture token 0123456789abcdef0123456789",
                "has characters other than visible ASCII",
            ),
            (CONNECTOR, "is the full connector token"),
        ] {
            assert_eq!(
                CaptureToken::parse(raw, CONNECTOR),
                CaptureToken::Unusable { reason },
                "{raw:?}"
            );
        }
    }

    #[test]
    fn resolve_capture_token_is_off_when_unset_and_judged_when_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(resolve_capture_token(CONNECTOR), CaptureToken::Off);
        crate::test_env::set_var(REMOTE_CAPTURE_TOKEN_ENV, "");
        assert_eq!(
            resolve_capture_token(CONNECTOR),
            CaptureToken::Unusable { reason: "is blank" }
        );
        crate::test_env::set_var(REMOTE_CAPTURE_TOKEN_ENV, CAPTURE);
        assert_eq!(resolve_capture_token(CONNECTOR).ready(), Some(CAPTURE));
        clear_env();
    }

    #[test]
    fn remote_status_reports_the_capture_token_state_never_its_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(REMOTE_TOKEN_ENV, CONNECTOR);
        crate::test_env::set_var(
            REMOTE_OAUTH_STATE_FILE_ENV,
            std::env::temp_dir().join(format!("rrm_capture_status_{}.json", std::process::id())),
        );
        assert_eq!(remote_status().capture_token, "off");
        crate::test_env::set_var(REMOTE_CAPTURE_TOKEN_ENV, CONNECTOR);
        assert_eq!(
            remote_status().capture_token,
            "unusable: REMIND_ME_REMOTE_CAPTURE_TOKEN is the full connector token"
        );
        crate::test_env::set_var(REMOTE_CAPTURE_TOKEN_ENV, CAPTURE);
        let status = remote_status();
        assert_eq!(status.capture_token, "ready");
        assert!(!serde_json::to_string(&status).unwrap().contains(CAPTURE));
        clear_env();
    }

    #[test]
    fn a_ready_capture_token_never_prints() {
        let shown = format!("{:?}", CaptureToken::Ready(CAPTURE.to_string()));
        assert!(!shown.contains(CAPTURE), "{shown}");
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
            .put_client("client-1", rusty_json::json!({"client_name": "claude.ai"}))
            .expect("write");
        assert_eq!(remote_status().oauth_clients, 1);

        let _ = fs::remove_dir_all(&dir);
        clear_env();
    }
}
