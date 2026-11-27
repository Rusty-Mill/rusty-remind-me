//! The client registry: who is calling, by a short stable id (ADR-0028 §1).
//!
//! `clients.json` sits beside the store (settings, not schema) and maps each
//! registered id to how a caller is recognised:
//!
//! ```json
//! {
//!   "claude-code":   { "aliases": ["claude-code"] },
//!   "claude-ai":     { "aliases": ["claude-ai"] },
//!   "hermes-jarvis": {},
//!   "dashboard":     { "api_keys": ["dashboard"] }
//! }
//! ```
//!
//! # Resolution, first match wins
//!
//! 1. the credential the caller authenticated with (a REST API key's *name*,
//!    or an OAuth client id), when the registry maps it;
//! 2. `REMIND_ME_CLIENT` from the caller's own config, as given;
//! 3. the MCP handshake name, when the registry lists it as an alias;
//! 4. `unknown`.
//!
//! This flips the old handshake-first rule: the handshake names the *app*,
//! and several bots share one app (every Hermes bot reports the same SDK
//! name). The handshake is still recorded, in `metadata.client_app`, so
//! nothing observed is lost.
//!
//! # Three registry states
//!
//! The registry feeds an identity, so it is never silently defaulted:
//!
//! - [`Registry::Off`]: no file. Today's precedence stays (handshake, then
//!   `REMIND_ME_CLIENT`, then `unknown`), so deploying this build changes no
//!   stamped value until someone writes the file.
//! - [`Registry::Unusable`]: a file that does not parse or validate. The
//!   ADR order applies with no aliases or credentials, so an unregistered
//!   handshake reads `unknown` rather than falling back to the old rule, and
//!   `remind_me_server_status` names the reason.
//! - [`Registry::Ready`]: the ADR order with the file's aliases and
//!   credentials.
//!
//! The file is re-read on every resolution, like `api_keys.json`, so an edit
//! takes effect without a restart. It is a few hundred bytes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Overrides where the registry is read from.
pub const CLIENTS_FILE_ENV: &str = "REMIND_ME_CLIENTS_FILE";
/// The registry's file name, beside the store.
pub const CLIENTS_FILE_NAME: &str = "clients.json";
/// What a caller nothing identifies is stamped as.
pub const UNKNOWN: &str = crate::sync::DEFAULT_CLIENT;
/// The client name the remote gate pins on every session opened with the
/// capture-only credential (ADR-0036, D67). It is a credential, not a
/// claim, so it stamps `client` in every registry state (D106).
pub const CAPTURE_CLIENT: &str = "remind-me-capture-token";
/// Longest client id.
pub const ID_MAX_LEN: usize = 40;

/// A registered client id: `[a-z0-9-]{1,40}`, and not `unknown`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ClientId(String);

impl ClientId {
    /// `raw` as an id.
    ///
    /// # Errors
    /// A message naming the rule `raw` breaks.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() || raw.len() > ID_MAX_LEN {
            return Err(format!(
                "client id {raw:?} must be 1 to {ID_MAX_LEN} characters"
            ));
        }
        let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if !raw.chars().all(allowed) {
            return Err(format!(
                "client id {raw:?} may hold only lowercase letters, digits and '-'"
            ));
        }
        if raw == UNKNOWN {
            return Err(format!("client id {UNKNOWN:?} is reserved"));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How one registered client is recognised. Every list may be empty: a
/// client identified only by its own `REMIND_ME_CLIENT` needs no entry
/// fields at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientEntry {
    /// MCP handshake names (`clientInfo.name`) that mean this client.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// OAuth client ids issued to this client by the remote connector.
    #[serde(default)]
    pub oauth_clients: Vec<String>,
    /// Names (never the secrets) of REST API keys this client holds.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// The tool profile this client sees (ADR-0028 §2), in the grammar of
    /// `REMIND_ME_TOOL_PROFILE`; beats it. Stored normalised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The fallback response format, in place of
    /// `REMIND_ME_DEFAULT_RESPONSE_FORMAT`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "format_any_case"
    )]
    pub format: Option<crate::models::ResponseFormat>,
    /// `remind_me_brief`'s `token_budget` when the caller sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_budget: Option<usize>,
}

/// `format` in any case (`"JSON"`, `" Markdown "`), as D126 allows; any
/// other value is still refused.
fn format_any_case<'de, D>(de: D) -> Result<Option<crate::models::ResponseFormat>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use crate::models::ResponseFormat;
    let raw = Option::<String>::deserialize(de)?;
    raw.map(|v| match v.trim().to_ascii_lowercase().as_str() {
        "markdown" => Ok(ResponseFormat::Markdown),
        "json" => Ok(ResponseFormat::Json),
        _ => Err(serde::de::Error::custom(format!(
            "format must be markdown or json, not {v:?}"
        ))),
    })
    .transpose()
}

/// The registry as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registry {
    /// No file: the pre-registry precedence holds.
    Off { path: PathBuf },
    /// A file that does not parse or validate.
    Unusable { path: PathBuf, reason: String },
    /// A valid registry.
    Ready {
        path: PathBuf,
        clients: BTreeMap<ClientId, ClientEntry>,
    },
}

/// What a caller authenticated with, when the transport knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// The name of a verified REST API key.
    ApiKey(String),
    /// An OAuth client id.
    OAuthClient(String),
}

/// Everything resolution reads about one caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    pub credential: Option<Credential>,
    /// `REMIND_ME_CLIENT` from the caller's own config.
    pub env: Option<String>,
    /// The handshake identity, `name` or `name/version`.
    pub handshake: Option<String>,
}

impl Registry {
    /// The registry at [`path`].
    pub fn load() -> Self {
        Self::load_from(&path())
    }

    /// The registry at `path`.
    pub fn load_from(path: &Path) -> Self {
        let path = path.to_path_buf();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::Off { path },
            Err(e) => {
                let reason = format!("cannot read: {e}");
                return Self::Unusable { path, reason };
            }
        };
        match parse(&raw) {
            Ok(clients) => Self::Ready { path, clients },
            Err(reason) => Self::Unusable { path, reason },
        }
    }

    /// Who `caller` is, by the rules in the module docs.
    pub fn resolve(&self, caller: &Caller) -> String {
        if let Some(pinned) = capture_pinned(caller) {
            return pinned.to_string();
        }
        let clients = match self {
            Self::Off { .. } => return legacy(caller),
            Self::Unusable { .. } => None,
            Self::Ready { clients, .. } => Some(clients),
        };
        let by_credential = caller
            .credential
            .as_ref()
            .and_then(|cred| clients.and_then(|c| find(c, |e| e.claims_credential(cred))));
        if let Some(id) = by_credential {
            return id.0.clone();
        }
        if let Some(env) = nonblank(caller.env.as_deref()) {
            return env.to_string();
        }
        let by_alias = nonblank(caller.handshake.as_deref())
            .and_then(|hs| clients.and_then(|c| find(c, |e| e.claims_handshake(hs))));
        by_alias.map_or_else(|| UNKNOWN.to_string(), |id| id.0.clone())
    }

    /// The entry `caller` resolves to, when the registry is ready and lists it.
    pub fn entry_for(&self, caller: &Caller) -> Option<&ClientEntry> {
        let Self::Ready { clients, .. } = self else {
            return None;
        };
        let id = ClientId::parse(&self.resolve(caller)).ok()?;
        clients.get(&id)
    }

    /// Number of registered clients (0 unless [`Registry::Ready`]).
    pub fn registered(&self) -> usize {
        match self {
            Self::Ready { clients, .. } => clients.len(),
            Self::Off { .. } | Self::Unusable { .. } => 0,
        }
    }
}

impl ClientEntry {
    fn claims_credential(&self, credential: &Credential) -> bool {
        match credential {
            Credential::ApiKey(name) => self.api_keys.iter().any(|k| k == name),
            Credential::OAuthClient(id) => self.oauth_clients.iter().any(|c| c == id),
        }
    }

    /// An alias matches the handshake's name, the part before any `/`,
    /// exactly (as the capture-tool exemption reads it, D68).
    fn claims_handshake(&self, handshake: &str) -> bool {
        let name = handshake_name(handshake);
        self.aliases.iter().any(|alias| alias == name)
    }
}

/// The handshake, when the capture-only gate pinned it (built-in
/// credential tier). Keeps its version, as it was stamped before.
fn capture_pinned(caller: &Caller) -> Option<&str> {
    nonblank(caller.handshake.as_deref()).filter(|hs| handshake_name(hs) == CAPTURE_CLIENT)
}

/// The name part of a handshake identity (`name/version`).
pub fn handshake_name(handshake: &str) -> &str {
    handshake.split('/').next().unwrap_or(handshake)
}

/// The pre-registry rule: handshake, then `REMIND_ME_CLIENT`, then `unknown`.
fn legacy(caller: &Caller) -> String {
    nonblank(caller.handshake.as_deref())
        .or_else(|| nonblank(caller.env.as_deref()))
        .unwrap_or(UNKNOWN)
        .to_string()
}

fn nonblank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

fn find(
    clients: &BTreeMap<ClientId, ClientEntry>,
    claims: impl Fn(&ClientEntry) -> bool,
) -> Option<&ClientId> {
    clients
        .iter()
        .find(|(_, entry)| claims(entry))
        .map(|(id, _)| id)
}

/// A registry file's clients, validated.
///
/// # Errors
/// Why the file is unusable: bad JSON, an unknown field, a bad id, a blank
/// alias or credential, or one claimed by two clients (which would make
/// resolution depend on map order).
fn parse(raw: &str) -> Result<BTreeMap<ClientId, ClientEntry>, String> {
    let file: BTreeMap<String, ClientEntry> =
        serde_json::from_str(raw).map_err(|e| format!("not a valid registry: {e}"))?;
    let mut clients = BTreeMap::new();
    let mut claimed: BTreeMap<String, ClientId> = BTreeMap::new();
    for (raw_id, mut entry) in file {
        let id = ClientId::parse(&raw_id)?;
        entry.profile = entry
            .profile
            .map(|raw| {
                crate::tool_profiles::normalise(&raw)
                    .ok_or_else(|| format!("client {raw_id:?} has an unknown profile {raw:?}"))
            })
            .transpose()?;
        if let Some(budget) = entry.brief_budget {
            crate::context_brief::check_token_budget(budget)
                .map_err(|e| format!("client {raw_id:?} brief_budget: {e}"))?;
        }
        let keys = entry
            .aliases
            .iter()
            .map(|a| ("alias", a))
            .chain(entry.oauth_clients.iter().map(|c| ("oauth client", c)))
            .chain(entry.api_keys.iter().map(|k| ("api key", k)));
        for (kind, value) in keys {
            if value.trim().is_empty() {
                return Err(format!("client {raw_id:?} has a blank {kind}"));
            }
            let key = format!("{kind}:{value}");
            if let Some(other) = claimed.insert(key, id.clone()) {
                if other != id {
                    return Err(format!(
                        "{kind} {value:?} is claimed by both {:?} and {raw_id:?}",
                        other.0
                    ));
                }
            }
        }
        clients.insert(id, entry);
    }
    Ok(clients)
}

/// Where the registry is read from: [`CLIENTS_FILE_ENV`], else
/// [`CLIENTS_FILE_NAME`] beside the store.
pub fn path() -> PathBuf {
    std::env::var(CLIENTS_FILE_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::db::resolve_memory_dir_child(CLIENTS_FILE_NAME))
}

/// The registry entry of the current caller (see [`crate::sync::current_caller`]),
/// when the registry is ready and lists the client it resolves to.
pub fn current_entry() -> Option<ClientEntry> {
    Registry::load()
        .entry_for(&crate::sync::current_caller())
        .cloned()
}

/// Memory rows inserted stamped `unknown` since [`COUNTING_SINCE`]. Counted
/// at the insert, so a provenance lookup that writes nothing is not counted
/// and a batch counts every row; a transaction rolled back after the insert
/// is not subtracted.
static UNKNOWN_WRITES: AtomicU64 = AtomicU64::new(0);
/// When this process started counting: the window `unknown_writes` covers.
static COUNTING_SINCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn counting_since() -> &'static str {
    COUNTING_SINCE.get_or_init(|| chrono::Utc::now().to_rfc3339())
}

/// Count one inserted memory row stamped `client`, when it is `unknown`.
pub fn note_write(client: &str) {
    counting_since();
    if client == UNKNOWN {
        UNKNOWN_WRITES.fetch_add(1, Ordering::Relaxed);
    }
}

/// The registry's state, for `remind_me_server_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientsStatus {
    /// `off`, `unusable` or `ready`.
    pub registry: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub registered: usize,
    /// Memory rows this process inserted stamped `unknown` since `counting_since`.
    pub unknown_writes: u64,
    /// RFC 3339 start of the `unknown_writes` window.
    pub counting_since: String,
}

/// The current registry's [`ClientsStatus`].
pub fn status() -> ClientsStatus {
    status_of(&Registry::load())
}

fn status_of(registry: &Registry) -> ClientsStatus {
    let (state, path, reason) = match registry {
        Registry::Off { path } => ("off", path, None),
        Registry::Unusable { path, reason } => ("unusable", path, Some(reason.clone())),
        Registry::Ready { path, .. } => ("ready", path, None),
    };
    ClientsStatus {
        registry: state.to_string(),
        path: path.display().to_string(),
        reason,
        registered: registry.registered(),
        unknown_writes: UNKNOWN_WRITES.load(Ordering::Relaxed),
        counting_since: counting_since().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(raw: &str) -> Registry {
        Registry::Ready {
            path: PathBuf::from("clients.json"),
            clients: parse(raw).expect("valid registry"),
        }
    }

    fn caller(cred: Option<Credential>, env: Option<&str>, hs: Option<&str>) -> Caller {
        Caller {
            credential: cred,
            env: env.map(str::to_string),
            handshake: hs.map(str::to_string),
        }
    }

    const REGISTRY: &str = r#"{
        "claude-code": { "aliases": ["claude-code"] },
        "dashboard":   { "api_keys": ["dash"] },
        "claude-ai":   { "oauth_clients": ["oc_1"], "aliases": ["claude-ai"] },
        "hermes-jarvis": {}
    }"#;

    #[test]
    fn ids_follow_the_pattern() {
        assert!(ClientId::parse("hermes-jarvis").is_ok());
        assert!(ClientId::parse(&"a".repeat(40)).is_ok());
        for bad in ["", "Hermes", "a_b", "a b", "unknown", &"a".repeat(41)] {
            assert!(ClientId::parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn credential_beats_env_and_handshake() {
        let reg = ready(REGISTRY);
        let key = Some(Credential::ApiKey("dash".into()));
        assert_eq!(
            reg.resolve(&caller(key, Some("x"), Some("claude-code/2"))),
            "dashboard"
        );
        let oauth = Some(Credential::OAuthClient("oc_1".into()));
        assert_eq!(reg.resolve(&caller(oauth, None, None)), "claude-ai");
    }

    #[test]
    fn unmapped_credential_falls_through() {
        let reg = ready(REGISTRY);
        let key = Some(Credential::ApiKey("other".into()));
        assert_eq!(
            reg.resolve(&caller(key, Some("hermes-jarvis"), None)),
            "hermes-jarvis"
        );
    }

    #[test]
    fn env_beats_handshake() {
        let reg = ready(REGISTRY);
        let got = reg.resolve(&caller(None, Some("hermes-jarvis"), Some("claude-code/2")));
        assert_eq!(got, "hermes-jarvis");
    }

    #[test]
    fn handshake_counts_only_as_a_registered_alias() {
        let reg = ready(REGISTRY);
        assert_eq!(
            reg.resolve(&caller(None, None, Some("claude-code/2.1"))),
            "claude-code"
        );
        assert_eq!(
            reg.resolve(&caller(None, None, Some("claude-code"))),
            "claude-code"
        );
        assert_eq!(
            reg.resolve(&caller(None, None, Some("claude-codex/1"))),
            UNKNOWN
        );
        assert_eq!(
            reg.resolve(&caller(None, None, Some("mcp-python/1"))),
            UNKNOWN
        );
        assert_eq!(reg.resolve(&caller(None, Some("  "), None)), UNKNOWN);
    }

    #[test]
    fn off_keeps_the_old_precedence() {
        let off = Registry::Off {
            path: PathBuf::from("x"),
        };
        assert_eq!(
            off.resolve(&caller(None, Some("env"), Some("app/1"))),
            "app/1"
        );
        assert_eq!(off.resolve(&caller(None, Some("env"), None)), "env");
        assert_eq!(off.resolve(&caller(None, None, None)), UNKNOWN);
    }

    #[test]
    fn unusable_applies_the_new_order_without_aliases() {
        let bad = Registry::Unusable {
            path: PathBuf::from("x"),
            reason: "bad".into(),
        };
        assert_eq!(
            bad.resolve(&caller(None, None, Some("claude-code/2"))),
            UNKNOWN
        );
        assert_eq!(
            bad.resolve(&caller(None, Some("env"), Some("app/1"))),
            "env"
        );
    }

    #[test]
    fn the_capture_credential_stamps_in_every_state() {
        let hs = Some("remind-me-capture-token/0");
        let off = Registry::Off {
            path: PathBuf::from("x"),
        };
        let bad = Registry::Unusable {
            path: PathBuf::from("x"),
            reason: "r".into(),
        };
        for reg in [ready(REGISTRY), off, bad] {
            let got = reg.resolve(&caller(None, Some("env"), hs));
            assert_eq!(got, "remind-me-capture-token/0");
        }
        let near =
            ready(REGISTRY).resolve(&caller(None, None, Some("remind-me-capture-token-x/0")));
        assert_eq!(near, UNKNOWN);
    }

    #[test]
    fn invalid_files_are_refused_with_a_reason() {
        for (raw, why) in [
            ("[]", "not a valid registry"),
            (r#"{"Bad": {}}"#, "lowercase"),
            (r#"{"unknown": {}}"#, "reserved"),
            (r#"{"a": {"profle": "core"}}"#, "unknown field"),
            (r#"{"a": {"profile": "cor"}}"#, "unknown profile"),
            (r#"{"a": {"format": "xml"}}"#, "markdown or json"),
            (r#"{"a": {"brief_budget": 5}}"#, "brief_budget"),
            (r#"{"a": {"aliases": [" "]}}"#, "blank alias"),
            (
                r#"{"a": {"aliases": ["x"]}, "b": {"aliases": ["x"]}}"#,
                "claimed by both",
            ),
        ] {
            let err = parse(raw).expect_err(raw);
            assert!(err.contains(why), "{raw}: {err}");
        }
    }

    #[test]
    fn profile_format_and_budget_are_read_per_client() {
        let reg = ready(
            r#"{
            "hermes-jarvis": { "profile": " Minimal ", "format": "JSON", "brief_budget": 600 },
            "claude-code": { "aliases": ["claude-code"], "format": " Markdown " }
        }"#,
        );
        let jarvis = reg
            .entry_for(&caller(None, Some("hermes-jarvis"), None))
            .expect("listed");
        assert_eq!(jarvis.profile.as_deref(), Some("minimal"));
        assert_eq!(jarvis.format, Some(crate::models::ResponseFormat::Json));
        assert_eq!(jarvis.brief_budget, Some(600));
        let code = reg
            .entry_for(&caller(None, None, Some("claude-code/2")))
            .expect("alias");
        assert_eq!(code.profile, None);
        assert_eq!(code.format, Some(crate::models::ResponseFormat::Markdown));
        assert!(reg.entry_for(&caller(None, Some("other"), None)).is_none());
        let off = Registry::Off {
            path: PathBuf::from("x"),
        };
        assert!(off
            .entry_for(&caller(None, Some("hermes-jarvis"), None))
            .is_none());
    }

    #[test]
    fn the_same_value_in_two_lists_is_not_a_clash() {
        assert!(parse(r#"{"a": {"aliases": ["x"], "api_keys": ["x"]}}"#).is_ok());
    }

    #[test]
    fn load_reports_each_state() {
        let dir = std::env::temp_dir().join(format!("clients_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join(CLIENTS_FILE_NAME);
        assert!(matches!(Registry::load_from(&file), Registry::Off { .. }));
        std::fs::write(&file, "{not json").expect("write");
        let unusable = Registry::load_from(&file);
        assert!(matches!(unusable, Registry::Unusable { .. }));
        assert_eq!(status_of(&unusable).registry, "unusable");
        std::fs::write(&file, REGISTRY).expect("write");
        let loaded = Registry::load_from(&file);
        assert_eq!(loaded.registered(), 4);
        let status = status_of(&loaded);
        assert_eq!((status.registry.as_str(), status.reason), ("ready", None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
