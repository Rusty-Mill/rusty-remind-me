//! Wire types for the OAuth authorization server (FT-07, `#86`) — RFC 7591
//! client registration/records and the RFC 6749 §5.1 token response.
//!
//! Metadata documents (RFC 8414 / RFC 9728) and error responses are built
//! as ad hoc `serde_json::json!` values in `routes.rs` instead of typed
//! structs here — matching this workspace's own convention throughout
//! `remind_me_mcp` (every tool response is a `json!` literal, not a
//! response struct) and avoiding a type for every one-shot response shape.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

fn default_grant_types() -> Vec<String> {
    vec![
        "authorization_code".to_string(),
        "refresh_token".to_string(),
    ]
}

fn default_response_types() -> Vec<String> {
    vec!["code".to_string()]
}

/// RFC 7591 §2 dynamic client registration request body.
///
/// Unknown/extra fields the reference's Pydantic model declares but this
/// single-user server never inspects (`jwks_uri`, `software_id`,
/// `contacts`, ...) are preserved via `extra` and echoed back verbatim in
/// the registration response — RFC 7591 §3.2.1 says the server "MUST
/// include" every registered metadata field in the response, so round-
/// tripping unknown fields through `extra` gets that for free instead of
/// hand-modeling every optional field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMetadata {
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default = "default_grant_types")]
    pub grant_types: Vec<String>,
    #[serde(default = "default_response_types")]
    pub response_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// RFC 7591 full client information record: what's persisted and what
/// `/register` returns. `client_secret`/`client_secret_expires_at` are
/// always `None` — see `provider::Provider::register_client`'s doc for why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInformation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Why a redirect_uri was rejected — RFC 6749 §3.1.2 redirect_uri pinning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectUriError {
    NotRegistered,
    AmbiguousWithoutOne,
}

impl ClientInformation {
    /// Pin a requested redirect_uri against this client's registered set
    /// (RFC 6749 §3.1.2). Mirrors the reference's
    /// `OAuthClientMetadata.validate_redirect_uri`: an explicit request must
    /// be one of the registered URIs; an omitted one is only unambiguous
    /// when exactly one is registered.
    pub fn validate_redirect_uri(
        &self,
        requested: Option<&str>,
    ) -> Result<String, RedirectUriError> {
        match requested {
            Some(uri) => {
                if self.redirect_uris.iter().any(|u| u == uri) {
                    Ok(uri.to_string())
                } else {
                    Err(RedirectUriError::NotRegistered)
                }
            }
            None => match self.redirect_uris.as_slice() {
                [only] => Ok(only.clone()),
                _ => Err(RedirectUriError::AmbiguousWithoutOne),
            },
        }
    }

    /// Validate a requested scope string against this client's registered
    /// `scope`. Mirrors the reference's `validate_scope`: `None` requested
    /// is always fine; otherwise every requested scope must be one the
    /// client registered with (an unset `self.scope` means the client
    /// registered with no scopes at all, so any non-empty request fails).
    pub fn validate_scope(&self, requested: Option<&str>) -> Result<Option<Vec<String>>, String> {
        let Some(requested) = requested else {
            return Ok(None);
        };
        let requested_scopes: Vec<String> = requested.split(' ').map(str::to_string).collect();
        let allowed: Vec<&str> = self
            .scope
            .as_deref()
            .map(|s| s.split(' ').collect())
            .unwrap_or_default();
        for scope in &requested_scopes {
            if !allowed.contains(&scope.as_str()) {
                return Err(format!("Client was not registered with scope {scope}"));
            }
        }
        Ok(Some(requested_scopes))
    }
}

/// RFC 6749 §5.1 access token response.
#[derive(Debug, Clone, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(redirect_uris: &[&str], scope: Option<&str>) -> ClientInformation {
        ClientInformation {
            client_id: Some("c1".to_string()),
            client_secret: None,
            client_id_issued_at: None,
            client_secret_expires_at: None,
            redirect_uris: redirect_uris.iter().map(|s| s.to_string()).collect(),
            token_endpoint_auth_method: "none".to_string(),
            grant_types: default_grant_types(),
            response_types: default_response_types(),
            scope: scope.map(str::to_string),
            client_name: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn redirect_uri_must_match_one_of_the_registered_uris_when_provided() {
        let c = client(&["https://a.example/cb", "https://b.example/cb"], None);
        assert_eq!(
            c.validate_redirect_uri(Some("https://a.example/cb")),
            Ok("https://a.example/cb".to_string())
        );
        assert_eq!(
            c.validate_redirect_uri(Some("https://evil.example/cb")),
            Err(RedirectUriError::NotRegistered)
        );
    }

    #[test]
    fn an_omitted_redirect_uri_only_resolves_when_exactly_one_is_registered() {
        let single = client(&["https://a.example/cb"], None);
        assert_eq!(
            single.validate_redirect_uri(None),
            Ok("https://a.example/cb".to_string())
        );

        let multiple = client(&["https://a.example/cb", "https://b.example/cb"], None);
        assert_eq!(
            multiple.validate_redirect_uri(None),
            Err(RedirectUriError::AmbiguousWithoutOne)
        );
    }

    #[test]
    fn scope_validation_passes_through_none_and_rejects_unregistered_scopes() {
        let c = client(&["https://a.example/cb"], Some("read write"));
        assert_eq!(c.validate_scope(None), Ok(None));
        assert_eq!(
            c.validate_scope(Some("read")),
            Ok(Some(vec!["read".to_string()]))
        );
        assert!(c.validate_scope(Some("admin")).is_err());
    }

    #[test]
    fn a_client_registered_with_no_scope_rejects_any_requested_scope() {
        let c = client(&["https://a.example/cb"], None);
        assert!(c.validate_scope(Some("read")).is_err());
    }
}
