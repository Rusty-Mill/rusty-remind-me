//! Secret redaction at the write boundary.
//!
//! A memory store is read back into prompts and synced to peers, so a pasted
//! credential would be replayed forever. This scrubs the common shapes before
//! anything is stored, replacing each with `[REDACTED:<kind>]`. Hand-rolled
//! (no `regex` dependency); every rule works on ASCII anchors so byte offsets
//! stay on char boundaries. `REMIND_ME_REDACT=0` opts out.

use serde_json::{json, Value};

/// Result of [`redact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub content: String,
    /// The kinds that fired, in rule order, deduplicated.
    pub hits: Vec<&'static str>,
}

/// Tag added to a memory whose content was redacted.
pub const REDACTED_TAG: &str = "redacted";

type Ranges = Vec<(usize, usize)>;
type Rule = (&'static str, fn(&str) -> Ranges);

const RULES: &[Rule] = &[
    ("private_key", pem_blocks),
    ("jwt", jwts),
    ("url_credentials", url_credentials),
    ("aws_access_key", aws_keys),
    ("github_token", github_tokens),
    ("slack_token", slack_tokens),
    ("secret", generic_secrets),
];

/// Whether redaction is on (`REMIND_ME_REDACT=0` turns it off).
pub fn enabled_by_env() -> bool {
    !matches!(std::env::var("REMIND_ME_REDACT").as_deref(), Ok("0"))
}

/// Redact every secret shape in `content`.
pub fn redact(content: &str) -> Redacted {
    let mut text = content.to_string();
    let mut hits = Vec::new();
    for (kind, find) in RULES {
        let ranges = find(&text);
        if ranges.is_empty() {
            continue;
        }
        text = replace(&text, &ranges, kind);
        hits.push(*kind);
    }
    Redacted {
        content: text,
        hits,
    }
}

/// The write-path entry point: redact `content` (unless disabled) and, when
/// anything fired, add the `redacted` tag and `metadata.redactions`.
pub fn guard(content: &str, tags: &mut Vec<String>, metadata: &mut Value) -> String {
    if !enabled_by_env() {
        return content.to_string();
    }
    let redacted = redact(content);
    if redacted.hits.is_empty() {
        return redacted.content;
    }
    if !tags.iter().any(|t| t == REDACTED_TAG) {
        tags.push(REDACTED_TAG.to_string());
    }
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata["redactions"] = json!(redacted.hits);
    redacted.content
}

fn replace(text: &str, ranges: &[(usize, usize)], kind: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for &(start, end) in ranges {
        out.push_str(&text[at..start]);
        out.push_str(&format!("[REDACTED:{kind}]"));
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Byte offsets of every occurrence of `needle`, not preceded by an
/// alphanumeric (so `xAKIA...` inside a longer token is left alone).
fn anchors<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let bytes = text.as_bytes();
    text.match_indices(needle)
        .map(|(i, _)| i)
        .filter(move |&i| i == 0 || !is_alnum(bytes[i - 1]))
}

/// Length of the run of bytes satisfying `ok` starting at `from`.
fn run(bytes: &[u8], from: usize, ok: fn(u8) -> bool) -> usize {
    bytes[from..].iter().take_while(|&&b| ok(b)).count()
}

fn aws_keys(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    anchors(text, "AKIA")
        .filter(|&i| {
            let tail = &bytes[i + 4..];
            tail.len() >= 16
                && tail[..16]
                    .iter()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
                && tail.get(16).is_none_or(|&b| !is_alnum(b))
        })
        .map(|i| (i, i + 20))
        .collect()
}

fn github_tokens(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        for i in anchors(text, prefix) {
            let n = run(bytes, i + 4, is_alnum);
            if n >= 36 {
                out.push((i, i + 4 + n));
            }
        }
    }
    out.sort_unstable();
    out
}

fn slack_tokens(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    for kind in ["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"] {
        for i in anchors(text, kind) {
            let n = run(bytes, i + 5, is_b64url);
            if n >= 10 {
                out.push((i, i + 5 + n));
            }
        }
    }
    out.sort_unstable();
    out
}

fn pem_blocks(text: &str) -> Ranges {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "PRIVATE KEY-----";
    let mut out = Ranges::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(BEGIN) {
        let start = from + rel;
        let label_at = start + BEGIN.len();
        let label_len = text[label_at..].find("-----").unwrap_or(0);
        if !text[label_at..label_at + label_len].ends_with("PRIVATE KEY") {
            from = label_at;
            continue;
        }
        let end = match text[start..].find("-----END ") {
            Some(e) => match text[start + e..].find(END) {
                Some(t) => start + e + t + END.len(),
                None => text.len(),
            },
            None => text.len(),
        };
        out.push((start, end));
        from = end;
    }
    out
}

fn jwts(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    let mut floor = 0;
    for i in anchors(text, "eyJ") {
        if i < floor {
            continue;
        }
        if let Some(end) = jwt_end(bytes, i) {
            out.push((i, end));
            floor = end;
        }
    }
    out
}

/// `eyJ<b64>.eyJ<b64>.<b64 signature, possibly empty>`.
fn jwt_end(bytes: &[u8], start: usize) -> Option<usize> {
    let first = run(bytes, start, is_b64url);
    let dot = start + first;
    if first < 10 || bytes.get(dot) != Some(&b'.') || !bytes[dot + 1..].starts_with(b"eyJ") {
        return None;
    }
    let second = run(bytes, dot + 1, is_b64url);
    let dot2 = dot + 1 + second;
    if bytes.get(dot2) != Some(&b'.') {
        return None;
    }
    Some(dot2 + 1 + run(bytes, dot2 + 1, is_b64url))
}

/// `scheme://user:pass@host`: only `pass` is replaced.
fn url_credentials(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    for (i, _) in text.match_indices("://") {
        let start = i + 3;
        let len = bytes[start..]
            .iter()
            .take_while(|&&b| !b.is_ascii_whitespace() && b != b'/' && b != b'@')
            .count();
        let at = start + len;
        if bytes.get(at) != Some(&b'@') {
            continue;
        }
        let Some(colon) = text[start..at].find(':') else {
            continue;
        };
        let pass = (start + colon + 1, at);
        if pass.0 < pass.1 && !text[pass.0..pass.1].starts_with("[REDACTED:") {
            out.push(pass);
        }
    }
    out
}

const KEYWORDS: &[&str] = &[
    "api_key", "api-key", "apikey", "secret", "token", "password",
];

/// `keyword [:=] value` with an eight-character-or-longer value; only the
/// value is replaced, so prose that merely mentions a "token" is untouched.
fn generic_secrets(text: &str) -> Ranges {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    let mut floor = 0;
    for word in KEYWORDS {
        for (i, _) in lower.match_indices(word) {
            let mut at = i + word.len();
            at += run(bytes, at, |b| b == b' ' || b == b'\t');
            if !matches!(bytes.get(at), Some(b':' | b'=')) {
                continue;
            }
            at += 1;
            at += run(bytes, at, |b| b == b' ' || b == b'\t');
            let value = &text[at..];
            let len = value
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map_or(value.len(), |(n, _)| n);
            let value = &value[..len];
            if value.chars().count() >= 8 && !value.starts_with("[REDACTED:") {
                out.push((at, at + len));
            }
        }
    }
    out.sort_unstable();
    out.retain(|&(s, e)| {
        let keep = s >= floor;
        if keep {
            floor = e;
        }
        keep
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<&'static str> {
        redact(text).hits
    }

    #[test]
    fn each_kind_is_redacted() {
        let github = format!("ghp_{}", "a1B2".repeat(9));
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc-DEF_123";
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIE\nabc\n-----END RSA PRIVATE KEY-----";
        let cases: Vec<(String, &str, &str)> = vec![
            ("key AKIAIOSFODNN7EXAMPLE end".into(), "aws_access_key", "key [REDACTED:aws_access_key] end"),
            (format!("t {github}."), "github_token", "t [REDACTED:github_token]."),
            ("xoxb-123456789012-abcdef".into(), "slack_token", "[REDACTED:slack_token]"),
            ("API_KEY=sk-live-12345678 x".into(), "secret", "API_KEY=[REDACTED:secret] x"),
            ("password: hunter2hunter2".into(), "secret", "password: [REDACTED:secret]"),
            (format!("jwt {jwt} end"), "jwt", "jwt [REDACTED:jwt] end"),
            (format!("a\n{pem}\nb"), "private_key", "a\n[REDACTED:private_key]\nb"),
            ("clone https://bob:s3cretpw@host.example/r.git".into(), "url_credentials", "clone https://bob:[REDACTED:url_credentials]@host.example/r.git"),
        ];
        for (input, kind, want) in cases {
            let r = redact(&input);
            assert_eq!(r.content, want, "{kind}");
            assert_eq!(r.hits, vec![kind], "{kind}");
        }
    }

    #[test]
    fn false_positives_are_left_alone() {
        for text in [
            "the token is rotated daily",
            "token: short",
            "password reset flow",
            "commit 3ac9f7971b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e merged",
            "AKIA is a prefix, AKIAshort",
            "https://example.org/a@b is fine",
            "https://example.org:8080/path",
            "xoxb- alone",
            "ghp_short",
            "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----",
        ] {
            assert!(kinds(text).is_empty(), "{text}");
            assert_eq!(redact(text).content, text);
        }
    }

    #[test]
    fn unterminated_pem_redacts_to_the_end() {
        let r = redact("x -----BEGIN PRIVATE KEY-----\nMIIabc");
        assert_eq!(r.content, "x [REDACTED:private_key]");
    }

    #[test]
    fn multiple_kinds_report_each_once() {
        let r = redact("token=abcdefgh1 and password=zzzzzzzz2 AKIAIOSFODNN7EXAMPLE");
        assert_eq!(r.hits, vec!["aws_access_key", "secret"]);
        assert_eq!(r.content.matches("[REDACTED:secret]").count(), 2);
    }

    #[test]
    fn guard_tags_and_records_metadata() {
        let mut tags = vec!["a".to_string()];
        let mut meta = json!({"k": 1});
        let out = guard("password=hunter2hunter2", &mut tags, &mut meta);
        assert_eq!(out, "password=[REDACTED:secret]");
        assert_eq!(tags, vec!["a", "redacted"]);
        assert_eq!(meta["redactions"], json!(["secret"]));
        assert_eq!(meta["k"], 1);
    }

    #[test]
    fn guard_leaves_clean_content_alone() {
        let mut tags = Vec::new();
        let mut meta = Value::Null;
        assert_eq!(guard("nothing here", &mut tags, &mut meta), "nothing here");
        assert!(tags.is_empty() && meta.is_null());
    }
}
