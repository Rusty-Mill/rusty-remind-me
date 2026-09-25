//! Issuer validation for the OAuth authorization server (FT-07, `#86`).
//!
//! `REMIND_ME_REMOTE_ISSUER` must be validated, never derived from the
//! inbound `Host` header: behind a tunnel the public hostname is not
//! knowable in advance, and `Host` is attacker-influenced (this is exactly
//! why `server::build_router` disables `rmcp`'s DNS-rebinding protection —
//! see that module's doc). The issuer is the one piece of OAuth metadata
//! that must instead come from operator-controlled configuration.

use std::fmt;

/// A validated `REMIND_ME_REMOTE_ISSUER`: an https origin (or http on
/// `localhost`/`127.0.0.1`, for local testing), no path beyond the root, no
/// query string, no fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issuer {
    origin: String,
}

impl Issuer {
    /// The canonical origin string (`scheme://host[:port]`, no trailing
    /// slash).
    pub fn as_str(&self) -> &str {
        &self.origin
    }

    /// `{origin}{path}` — for building endpoint URLs in AS metadata.
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
}

impl fmt::Display for Issuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

/// Why an issuer was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuerError {
    NotAUrl,
    UnsupportedScheme,
    NotHttps,
    HasPath(String),
    HasQuery,
    HasFragment,
}

impl fmt::Display for IssuerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAUrl => write!(f, "issuer is not a valid absolute URL"),
            Self::UnsupportedScheme => write!(f, "issuer must use http or https"),
            Self::NotHttps => write!(
                f,
                "issuer URL must be HTTPS (http is only allowed for localhost/127.0.0.1)"
            ),
            Self::HasPath(p) => write!(f, "issuer URL must not have a path (found {p:?})"),
            Self::HasQuery => write!(f, "issuer URL must not have a query string"),
            Self::HasFragment => write!(f, "issuer URL must not have a fragment"),
        }
    }
}

impl std::error::Error for IssuerError {}

/// Parse and validate `raw` as an OAuth issuer.
///
/// Combines two checks from the reference exactly as `remote.py`'s
/// `build_remote_app` applies them together: the installed MCP SDK's
/// `validate_issuer_url` (scheme must be `https`, unless the host is
/// `localhost` or starts with `127.0.0.1` — RFC 8414 requires HTTPS, the SDK
/// carves out an exception for local testing; no fragment, no query) plus
/// `remote.py`'s own additional rule that the path must be empty or exactly
/// `/` (an issuer is an *origin*, not a URL with a meaningful path).
///
/// This is a small hand-rolled parser rather than a general-purpose URL
/// crate: the workspace has no `url`-equivalent dependency, and the surface
/// this needs (scheme / authority / path / query / fragment, nothing else —
/// no userinfo, no percent-decoding) is narrow enough that a dedicated
/// parser is both simpler and easier to audit than pulling in one.
pub fn validate_issuer(raw: &str) -> Result<Issuer, IssuerError> {
    let (before_fragment, has_fragment) = match raw.split_once('#') {
        Some((before, _)) => (before, true),
        None => (raw, false),
    };
    let (before_query, has_query) = match before_fragment.split_once('?') {
        Some((before, _)) => (before, true),
        None => (before_fragment, false),
    };
    let (scheme, rest) = before_query.split_once("://").ok_or(IssuerError::NotAUrl)?;
    if scheme != "http" && scheme != "https" {
        return Err(IssuerError::UnsupportedScheme);
    }
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(IssuerError::NotAUrl);
    }
    let host = host_of(authority);
    if scheme != "https" && host != "localhost" && !host.starts_with("127.0.0.1") {
        return Err(IssuerError::NotHttps);
    }
    if !path.is_empty() && path != "/" {
        return Err(IssuerError::HasPath(path.to_string()));
    }
    // Checked last, matching the reference's own ordering (validate_issuer_url
    // raises on scheme before fragment/query), so the error a malformed
    // issuer surfaces is deterministic across equivalent inputs.
    if has_query {
        return Err(IssuerError::HasQuery);
    }
    if has_fragment {
        return Err(IssuerError::HasFragment);
    }
    Ok(Issuer {
        origin: format!("{scheme}://{authority}"),
    })
}

/// Extract the lowercased host portion of an authority (`host[:port]` or
/// `[ipv6]`/`[ipv6]:port`).
fn host_of(authority: &str) -> String {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest).to_string()
    } else {
        match authority.rsplit_once(':') {
            // Only strip a trailing `:port` when what follows is all
            // digits -- a bare IPv4/hostname authority never contains a
            // colon, so this is unambiguous for the inputs this function
            // actually sees.
            Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                host.to_string()
            }
            _ => authority.to_string(),
        }
    };
    host.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_https_origin_is_valid() {
        let issuer = validate_issuer("https://machine.tailnet.ts.net").unwrap();
        assert_eq!(issuer.as_str(), "https://machine.tailnet.ts.net");
        assert_eq!(
            issuer.endpoint("/authorize"),
            "https://machine.tailnet.ts.net/authorize"
        );
    }

    #[test]
    fn a_trailing_slash_is_equivalent_to_no_path() {
        let issuer = validate_issuer("https://machine.tailnet.ts.net/").unwrap();
        assert_eq!(issuer.as_str(), "https://machine.tailnet.ts.net");
    }

    #[test]
    fn http_is_rejected_for_a_non_local_host() {
        assert_eq!(
            validate_issuer("http://machine.example"),
            Err(IssuerError::NotHttps)
        );
    }

    #[test]
    fn http_is_allowed_for_localhost_and_127_0_0_1() {
        assert!(validate_issuer("http://localhost:8768").is_ok());
        assert!(validate_issuer("http://127.0.0.1:8768").is_ok());
        assert!(validate_issuer("http://127.0.0.1").is_ok());
    }

    #[test]
    fn a_path_beyond_the_root_is_rejected() {
        assert_eq!(
            validate_issuer("https://machine.example/path"),
            Err(IssuerError::HasPath("/path".to_string()))
        );
    }

    #[test]
    fn a_query_string_is_rejected() {
        assert_eq!(
            validate_issuer("https://machine.example?x=1"),
            Err(IssuerError::HasQuery)
        );
    }

    #[test]
    fn a_fragment_is_rejected() {
        assert_eq!(
            validate_issuer("https://machine.example#frag"),
            Err(IssuerError::HasFragment)
        );
    }

    #[test]
    fn an_unparseable_or_schemeless_value_is_rejected() {
        assert_eq!(validate_issuer("not a url"), Err(IssuerError::NotAUrl));
        assert_eq!(validate_issuer(""), Err(IssuerError::NotAUrl));
        assert_eq!(
            validate_issuer("ftp://machine.example"),
            Err(IssuerError::UnsupportedScheme)
        );
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        assert!(validate_issuer("http://LOCALHOST:8768").is_ok());
    }
}
