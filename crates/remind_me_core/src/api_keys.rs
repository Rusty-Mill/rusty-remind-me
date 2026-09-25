//! Named, scope-limited dashboard API keys.
//!
//! # The scope enforcement is the feature
//!
//! Issuing a key is a few lines. What matters is that a `read` key is
//! *actually* refused on every mutating route, because a read-scoped key that
//! can still write is worse than not having the feature at all — it gets
//! handed to someone on the understanding that it is safe.
//!
//! This is **not** multi-tenancy. Every key reads and writes the same vault;
//! only the permitted methods differ. There is no per-key data partitioning
//! and none is implied.
//!
//! # Storage discipline
//!
//! - A JSON file, `0600`, beside the other secrets this crate persists.
//! - **Only the SHA-256 hash is stored, never the plaintext.** A key is
//!   readable exactly once, at issuance; after that it can be revoked and
//!   replaced but never recovered.
//! - Written by atomic replace through a sibling temp file. Truncating in
//!   place means a crash mid-write leaves a partial file, which the next read
//!   treats as empty — and the next write would then persist that emptiness,
//!   silently revoking every key.
//! - Re-read on every operation rather than cached, so a key issued or revoked
//!   by another process takes effect here immediately. A cached store would
//!   keep honouring a key revoked seconds ago in a different terminal.
//! - Verified with a constant-time comparison over fixed-length hex digests,
//!   so timing leaks nothing about which key came close.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const API_KEYS_FILE_ENV: &str = "REMIND_ME_API_KEYS_FILE";

/// The name reserved for the flat `REMIND_ME_API_KEY`. That key is
/// config-managed rather than stored here, and is always read-write — which is
/// what it was before scopes existed, so adding scopes cannot retroactively
/// restrict a deployment that already relies on it.
pub const DEFAULT_KEY_NAME: &str = "default";

pub const SCOPE_READ: &str = "read";
pub const SCOPE_READ_WRITE: &str = "read-write";
pub const SCOPES: [&str; 2] = [SCOPE_READ, SCOPE_READ_WRITE];

/// Methods a `read`-scoped key may not use.
pub const MUTATING_METHODS: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];

/// One stored key. `key_hash` never leaves this module.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredKey {
    name: String,
    key_hash: String,
    scope: String,
    created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KeyFile {
    #[serde(default)]
    keys: Vec<StoredKey>,
}

/// What `list` returns: everything except anything secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    pub name: String,
    pub scope: String,
    pub created_at: String,
}

/// A verified caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedKey {
    pub name: String,
    pub scope: String,
}

impl VerifiedKey {
    /// Whether this key may use `method`.
    ///
    /// Unknown scopes are treated as read-only rather than read-write. A
    /// hand-edited or future-versioned file must fail closed: the alternative
    /// grants write access on a typo.
    pub fn may_use(&self, method: &str) -> bool {
        if self.scope == SCOPE_READ_WRITE {
            return true;
        }
        !MUTATING_METHODS.contains(&method.to_ascii_uppercase().as_str())
    }
}

#[derive(Debug)]
pub enum ApiKeyError {
    /// The name was blank, reserved, or already taken; or the scope is unknown.
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for ApiKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Io(m) => write!(f, "{}", m),
        }
    }
}
impl std::error::Error for ApiKeyError {}

/// `~/.remind-me/api_keys.json`, alongside [`crate::db::DEFAULT_DIR_NAME`].
fn default_store_path() -> PathBuf {
    crate::db::resolve_memory_dir_child("api_keys.json")
}

pub fn store_path() -> PathBuf {
    std::env::var(API_KEYS_FILE_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(default_store_path)
}

fn hash_key(plaintext: &str) -> String {
    sha256::digest(plaintext)
}

/// Read the store. A missing or unreadable file reads as empty.
///
/// Deliberately not an error: a node that has never issued a key has no file,
/// and that is the ordinary case rather than a fault. An unreadable one is
/// reported and treated as empty so a corrupt file cannot wedge the whole API
/// — it fails closed, since an empty store authorises nothing.
fn read_keys() -> Vec<StoredKey> {
    let path = store_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<KeyFile>(&raw) {
        Ok(file) => file.keys,
        Err(e) => {
            eprintln!(
                "api_keys: could not parse {} ({}); treating as empty",
                path.display(),
                e
            );
            Vec::new()
        }
    }
}

/// Persist by atomic replace, `0600`.
fn write_keys(keys: &[StoredKey]) -> Result<(), ApiKeyError> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApiKeyError::Io(format!("creating {}: {}", parent.display(), e)))?;
    }

    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let body = serde_json::to_string_pretty(&KeyFile {
        keys: keys.to_vec(),
    })
    .map_err(|e| ApiKeyError::Io(e.to_string()))?;

    std::fs::write(&temp, body + "\n")
        .map_err(|e| ApiKeyError::Io(format!("writing {}: {}", temp.display(), e)))?;

    // Tightened before the rename, not after: between a world-readable create
    // and a later chmod there is a window in which the hashes are readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&temp);
            return Err(ApiKeyError::Io(format!(
                "securing {}: {}",
                temp.display(),
                e
            )));
        }
    }

    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        ApiKeyError::Io(format!("replacing {}: {}", path.display(), e))
    })
}

/// Every stored key's name, scope and creation time — never a hash, never a
/// plaintext.
pub fn list_keys() -> Vec<ApiKeyInfo> {
    read_keys()
        .into_iter()
        .map(|k| ApiKeyInfo {
            name: k.name,
            scope: k.scope,
            created_at: k.created_at,
        })
        .collect()
}

/// Issue a key, returning the plaintext **once**.
///
/// Only the hash is persisted, so this return value is the only time the key
/// exists in readable form anywhere.
pub fn create_key(name: &str, scope: &str) -> Result<String, ApiKeyError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiKeyError::Invalid("name is required".to_string()));
    }
    if name == DEFAULT_KEY_NAME {
        return Err(ApiKeyError::Invalid(format!(
            "'{}' is reserved for the REMIND_ME_API_KEY / auto-generated key",
            DEFAULT_KEY_NAME
        )));
    }
    if !SCOPES.contains(&scope) {
        return Err(ApiKeyError::Invalid(format!(
            "scope must be one of {:?}, got '{}'",
            SCOPES, scope
        )));
    }

    let mut keys = read_keys();
    if keys.iter().any(|k| k.name == name) {
        return Err(ApiKeyError::Invalid(format!(
            "a key named '{}' already exists",
            name
        )));
    }

    let plaintext = crate::remote::generate_token();
    keys.push(StoredKey {
        name: name.to_string(),
        key_hash: hash_key(&plaintext),
        scope: scope.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    });
    write_keys(&keys)?;
    Ok(plaintext)
}

/// Revoke a named key. `false` when no such key existed.
pub fn revoke_key(name: &str) -> Result<bool, ApiKeyError> {
    if name == DEFAULT_KEY_NAME {
        return Err(ApiKeyError::Invalid(format!(
            "'{}' is the REMIND_ME_API_KEY / auto-generated key — it is \
             config-managed, not revocable here. Unset REMIND_ME_API_KEY instead.",
            DEFAULT_KEY_NAME
        )));
    }
    let keys = read_keys();
    let remaining: Vec<StoredKey> = keys.iter().filter(|k| k.name != name).cloned().collect();
    if remaining.len() == keys.len() {
        return Ok(false);
    }
    write_keys(&remaining)?;
    Ok(true)
}

/// Verify a presented plaintext against every stored hash.
///
/// Compares fixed-length hex digests in constant time, so timing reveals
/// nothing about which key nearly matched. Every key is checked even after a
/// match, so the work does not depend on position in the file either.
pub fn verify(presented: &str) -> Option<VerifiedKey> {
    let presented_hash = hash_key(presented);
    let mut found: Option<VerifiedKey> = None;
    for key in read_keys() {
        if crate::webhook::constant_time_eq(key.key_hash.as_bytes(), presented_hash.as_bytes()) {
            found.get_or_insert(VerifiedKey {
                name: key.name,
                scope: key.scope,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_store_path_uses_the_hyphenated_data_directory() {
        // Regression: this used to hardcode `.remind_me` (underscored), a
        // directory nothing else in this port reads or writes -- see
        // `remote::default_token_file`'s doc for the same fix applied there.
        let path = default_store_path();
        assert_eq!(path.file_name().unwrap(), "api_keys.json");
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            crate::db::DEFAULT_DIR_NAME
        );
    }
}
