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
    ("gitlab_token", gitlab_tokens),
    ("slack_token", slack_tokens),
    ("llm_api_key", llm_api_keys),
    ("stripe_key", stripe_keys),
    ("npm_token", npm_tokens),
    ("pypi_token", pypi_tokens),
    ("google_api_key", google_api_keys),
    ("card_number", card_numbers),
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

/// Every `prefix` followed by at least `min` bytes satisfying `ok`; the
/// range covers the prefix and the whole run.
fn prefixed(text: &str, prefixes: &[&str], ok: fn(u8) -> bool, min: usize) -> Ranges {
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    for prefix in prefixes {
        for i in anchors(text, prefix) {
            let n = run(bytes, i + prefix.len(), ok);
            if n >= min {
                out.push((i, i + prefix.len() + n));
            }
        }
    }
    out.sort_unstable();
    out
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn github_tokens(text: &str) -> Ranges {
    let mut out = prefixed(
        text,
        &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        is_alnum,
        36,
    );
    // Fine-grained PATs: `github_pat_` + 82 word characters.
    out.extend(prefixed(text, &["github_pat_"], is_word, 60));
    out.sort_unstable();
    out
}

fn gitlab_tokens(text: &str) -> Ranges {
    prefixed(text, &["glpat-", "gldt-", "glrt-"], is_b64url, 20)
}

fn slack_tokens(text: &str) -> Ranges {
    prefixed(
        text,
        &["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"],
        is_b64url,
        10,
    )
}

/// OpenAI (`sk-`, `sk-proj-`) and Anthropic (`sk-ant-`) keys. Real keys run
/// to 40+ characters; the floor of 32 leaves `sk-live-1234` style prose to
/// the generic rule.
fn llm_api_keys(text: &str) -> Ranges {
    prefixed(text, &["sk-"], is_b64url, 32)
}

fn stripe_keys(text: &str) -> Ranges {
    let prefixes = ["sk_live_", "sk_test_", "rk_live_", "rk_test_", "whsec_"];
    prefixed(text, &prefixes, is_alnum, 16)
}

fn npm_tokens(text: &str) -> Ranges {
    prefixed(text, &["npm_"], is_alnum, 36)
}

/// PyPI API tokens are macaroons whose base64 always opens with the
/// encoded `pypi.org` location.
fn pypi_tokens(text: &str) -> Ranges {
    prefixed(text, &["pypi-AgEIcHlwaS5vcmc"], is_b64url, 30)
}

/// `AIza` + exactly 35 base64url characters.
fn google_api_keys(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    anchors(text, "AIza")
        .filter(|&i| run(bytes, i + 4, is_b64url) == 35)
        .map(|i| (i, i + 39))
        .collect()
}

/// Payment card numbers: 13-19 digits, optionally grouped by single spaces
/// or hyphens, with a known network prefix and length and a valid Luhn
/// check digit. The prefix and length rules keep millisecond timestamps
/// and other long ids out.
fn card_numbers(text: &str) -> Ranges {
    let bytes = text.as_bytes();
    let mut out = Ranges::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() || (i > 0 && is_alnum(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let end = digit_span_end(bytes, i);
        let digits: Vec<u8> = bytes[i..end]
            .iter()
            .filter(|b| b.is_ascii_digit())
            .map(|b| b - b'0')
            .collect();
        let bounded = bytes.get(end).is_none_or(|&b| !is_alnum(b));
        if bounded && is_card(&digits) {
            out.push((i, end));
        }
        i = end.max(i + 1);
    }
    out
}

/// End of a run of digits joined by single ` ` or `-` separators.
fn digit_span_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start + run(bytes, start, |b| b.is_ascii_digit());
    while matches!(bytes.get(end), Some(b' ' | b'-'))
        && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)
    {
        end += 1 + run(bytes, end + 1, |b| b.is_ascii_digit());
    }
    end
}

fn is_card(digits: &[u8]) -> bool {
    let len = digits.len();
    let prefix = |n: usize| {
        digits
            .iter()
            .take(n)
            .fold(0u32, |acc, &d| acc * 10 + u32::from(d))
    };
    let network = match digits.first() {
        Some(4) => matches!(len, 13 | 16 | 19),
        Some(3) => matches!(prefix(2), 34 | 37) && len == 15,
        Some(5) => (51..=55).contains(&prefix(2)) && len == 16,
        Some(2) => (2221..=2720).contains(&prefix(4)) && len == 16,
        Some(6) => {
            (prefix(4) == 6011 || prefix(2) == 65 || (644..=649).contains(&prefix(3)))
                && (16..=19).contains(&len)
        }
        _ => false,
    };
    network && luhn(digits)
}

fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(n, &d)| {
            let d = u32::from(d);
            match n % 2 {
                0 => d,
                _ if d > 4 => d * 2 - 9,
                _ => d * 2,
            }
        })
        .sum();
    sum.is_multiple_of(10)
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
            (
                "key AKIAIOSFODNN7EXAMPLE end".into(),
                "aws_access_key",
                "key [REDACTED:aws_access_key] end",
            ),
            (
                format!("t {github}."),
                "github_token",
                "t [REDACTED:github_token].",
            ),
            (
                "xoxb-123456789012-abcdef".into(),
                "slack_token",
                "[REDACTED:slack_token]",
            ),
            (
                "API_KEY=sk-live-12345678 x".into(),
                "secret",
                "API_KEY=[REDACTED:secret] x",
            ),
            (
                "password: hunter2hunter2".into(),
                "secret",
                "password: [REDACTED:secret]",
            ),
            (format!("jwt {jwt} end"), "jwt", "jwt [REDACTED:jwt] end"),
            (
                format!("a\n{pem}\nb"),
                "private_key",
                "a\n[REDACTED:private_key]\nb",
            ),
            (
                "clone https://bob:s3cretpw@host.example/r.git".into(),
                "url_credentials",
                "clone https://bob:[REDACTED:url_credentials]@host.example/r.git",
            ),
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

    /// Token fixtures are assembled at run time so no literal key shape sits
    /// in the source for a push-time secret scanner to flag.
    #[test]
    fn provider_tokens_are_redacted() {
        let body = |n: usize| "aB3d".repeat(n.div_ceil(4))[..n].to_string();
        let cases: Vec<(String, &str)> = vec![
            (format!("github_pat_{}", body(82)), "github_token"),
            (format!("glpat-{}", body(20)), "gitlab_token"),
            (format!("sk-{}", body(48)), "llm_api_key"),
            (format!("sk-proj-{}", body(64)), "llm_api_key"),
            (format!("sk-ant-api03-{}", body(90)), "llm_api_key"),
            (format!("sk_live_{}", body(24)), "stripe_key"),
            (format!("rk_test_{}", body(24)), "stripe_key"),
            (format!("whsec_{}", body(32)), "stripe_key"),
            (format!("npm_{}", body(36)), "npm_token"),
            (format!("pypi-AgEIcHlwaS5vcmc{}", body(60)), "pypi_token"),
            (format!("AIza{}", body(35)), "google_api_key"),
        ];
        for (token, kind) in cases {
            let r = redact(&format!("use {token} now"));
            assert_eq!(r.content, format!("use [REDACTED:{kind}] now"), "{kind}");
            assert_eq!(r.hits, vec![kind], "{kind}");
        }
    }

    #[test]
    fn card_numbers_with_a_network_prefix_and_luhn_are_redacted() {
        for card in [
            "4111111111111111",
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
            "5555555555554444",
            "2223003122003222",
            "378282246310005",
            "6011111111111117",
        ] {
            let r = redact(&format!("card {card}, exp 12/30"));
            assert_eq!(
                r.content, "card [REDACTED:card_number], exp 12/30",
                "{card}"
            );
            assert_eq!(r.hits, vec!["card_number"]);
        }
    }

    #[test]
    fn connection_strings_lose_their_password() {
        for (scheme, host) in [
            ("postgres", "db:5432/app"),
            ("mysql", "db:3306/app"),
            ("mongodb+srv", "cluster0.example.net/app"),
        ] {
            let r = redact(&format!("{scheme}://app:s3cretpw@{host}"));
            assert_eq!(
                r.content,
                format!("{scheme}://app:[REDACTED:url_credentials]@{host}")
            );
        }
    }

    #[test]
    fn new_shapes_leave_ordinary_text_alone() {
        for text in [
            "created_ms 1793289844375 and 1793235024256",
            "order 4111111111111112 fails Luhn",
            "id 1234567812345670 has no network prefix",
            "4111111111111111x is part of a word",
            "phone 555-123-4567, zip 90210",
            "sk-short and a task-list",
            "risk-based ask-me-anything prompts",
            "AIza is a prefix, AIzaShort",
            "npm_modules and npm_config_cache",
            "sk_live_ only",
            "glpat- alone",
        ] {
            assert!(kinds(text).is_empty(), "{text}");
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
