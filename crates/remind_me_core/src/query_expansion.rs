//! Optional HyDE query expansion for semantic search.
//!
//! The weakest retrieval cases are questions phrased nothing like the memory
//! that answers them (short scattered preferences, multi-hop temporal
//! questions). HyDE (Hypothetical Document Embeddings) bridges that gap: a
//! small local LLM writes a short passage that *would* answer the question,
//! and the passage's embedding — which lives in document-space, not
//! question-space — is averaged with the query's before the vector search
//! (see [`crate::vectors::fuse_query_embedding`]).
//!
//! Off by default. Enable with `REMIND_ME_QUERY_EXPANSION=hyde`; generation
//! uses the same local Ollama daemon the Ollama embedding backend talks to
//! (`REMIND_ME_OLLAMA_URL`). Any failure (daemon down, model missing,
//! timeout) silently falls back to the plain query — expansion must never
//! break or slow search beyond its configured timeout. Matches the
//! reference's `remind_me_mcp/query_expansion.py`.
//!
//! # Deliberate simplification vs. the reference
//!
//! The reference coalesces concurrent callers racing the *same* uncached
//! query behind a per-query `threading.Event`, so two threads never both
//! pay a redundant blocking Ollama generation for one query. That is a
//! resource-use optimization, not an observable-behavior difference (every
//! caller still gets a correct result either way), so this port omits it:
//! a cache-only bounded LRU, no in-flight coalescing. Revisit if concurrent
//! duplicate HyDE calls show up as a real cost.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

/// Query expansion mode: unset/empty (disabled, the default) or `"hyde"`.
pub const EXPANSION_MODE_ENV: &str = "REMIND_ME_QUERY_EXPANSION";
/// Ollama model used to write the hypothetical passage. Small instruct
/// models work well.
pub const HYDE_MODEL_ENV: &str = "REMIND_ME_HYDE_MODEL";
pub const DEFAULT_HYDE_MODEL: &str = "llama3.2";
/// Seconds to wait for the generation before falling back to the plain
/// query.
pub const HYDE_TIMEOUT_ENV: &str = "REMIND_ME_HYDE_TIMEOUT";
pub const DEFAULT_HYDE_TIMEOUT_SECS: f64 = 15.0;

/// Passage cap — one embedding window; longer adds noise, not signal.
const HYDE_MAX_CHARS: usize = 600;
/// Successful expansions cached per query, bounded LRU.
const EXPANSION_CACHE_MAX: usize = 128;

const HYDE_PROMPT_PREFIX: &str =
    "Write a short passage (2-3 sentences) that could plausibly appear in a \
     personal conversation log and that directly answers the question below. \
     Invent plausible specifics. Output only the passage.\n\nQuestion: ";
const HYDE_PROMPT_SUFFIX: &str = "\n\nPassage:";

fn hyde_prompt(query: &str) -> String {
    format!("{HYDE_PROMPT_PREFIX}{query}{HYDE_PROMPT_SUFFIX}")
}

/// Whether the *setting* asks for HyDE expansion.
pub fn enabled() -> bool {
    std::env::var(EXPANSION_MODE_ENV)
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("hyde")
}

fn hyde_model() -> String {
    std::env::var(HYDE_MODEL_ENV).unwrap_or_else(|_| DEFAULT_HYDE_MODEL.to_string())
}

fn hyde_timeout() -> Duration {
    let secs = std::env::var(HYDE_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(DEFAULT_HYDE_TIMEOUT_SECS);
    Duration::from_secs_f64(secs)
}

/// Truncate to at most `max_chars` **characters** (not bytes), matching the
/// reference's Python slicing.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// One non-streaming Ollama `/api/generate` call. Hand-rolled over
/// `std::net`, the same minimal-dependency approach
/// [`crate::embedder::OllamaEmbedder`] uses for `/api/embed` — this is its
/// own small client rather than a shared one so a change to either HTTP
/// call cannot accidentally affect the other's already-tested behavior.
fn generate(model: &str, prompt: &str, timeout: Duration) -> Result<String, String> {
    let url = std::env::var(crate::embedder::OLLAMA_URL_ENV)
        .unwrap_or_else(|_| crate::embedder::DEFAULT_OLLAMA_URL.to_string());
    let url = url.trim_end_matches('/');
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// Ollama URLs are supported, got {url:?}"))?;
    let (host, port) = rest
        .split_once(':')
        .ok_or_else(|| format!("Ollama URL has no port: {url:?}"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("invalid port in Ollama URL: {url:?}"))?;

    use std::net::ToSocketAddrs;
    let addr = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| format!("cannot resolve Ollama host {host:?}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("cannot reach Ollama at {url}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let body = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.0 },
    })
    .to_string();
    let request = format!(
        "POST /api/generate HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("writing to Ollama failed: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("reading from Ollama failed: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, response_body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response from Ollama".to_string())?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "malformed HTTP status line from Ollama".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "Ollama returned {status}: {}",
            response_body.trim()
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(response_body)
        .map_err(|e| format!("Ollama response was not JSON: {e}"))?;
    Ok(parsed
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Generate a hypothetical answer passage for `query`, or `None` on any
/// failure (daemon down, model missing, timeout, empty response). Broad
/// failure tolerance is intentional: expansion is an enhancement, never a
/// reason a search should be slower-failing than the plain-query path.
pub fn hyde_passage(query: &str) -> Option<String> {
    let passage = generate(&hyde_model(), &hyde_prompt(query), hyde_timeout())
        .ok()?
        .trim()
        .to_string();
    let truncated = truncate_chars(&passage, HYDE_MAX_CHARS);
    (!truncated.is_empty()).then_some(truncated)
}

static EXPANSION_CACHE: Mutex<Vec<(String, Vec<String>)>> = Mutex::new(Vec::new());

fn cache_get(query: &str) -> Option<Vec<String>> {
    let mut cache = EXPANSION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let pos = cache.iter().position(|(q, _)| q == query)?;
    let entry = cache.remove(pos);
    let texts = entry.1.clone();
    cache.push(entry); // move to back (most-recently-used)
    Some(texts)
}

fn cache_put(query: &str, texts: Vec<String>) {
    let mut cache = EXPANSION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(q, _)| q != query);
    cache.push((query.to_string(), texts));
    while cache.len() > EXPANSION_CACHE_MAX {
        cache.remove(0);
    }
}

/// Return extra retrieval texts for `query` per the configured mode.
///
/// The returned texts are meant to be embedded alongside the query and
/// averaged into a single search vector (see
/// [`crate::vectors::fuse_query_embedding`]). Disabled mode returns `[]`,
/// which leaves the search vector exactly the query embedding. Successful
/// expansions are cached (bounded LRU) per query.
pub fn expand_query(query: &str) -> Vec<String> {
    if !enabled() || query.trim().is_empty() {
        return Vec::new();
    }
    if let Some(cached) = cache_get(query) {
        return cached;
    }
    match hyde_passage(query) {
        Some(passage) => {
            let texts = vec![passage];
            cache_put(query, texts.clone());
            texts
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // EXPANSION_MODE_ENV is process-global; serialize this module's
    // env-touching tests so they don't race each other the way `cargo test`
    // otherwise would (see retrieval.rs's own ENV_LOCK for the established
    // convention).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_by_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::remove_var(EXPANSION_MODE_ENV);
        assert!(!enabled());
    }

    #[test]
    fn mode_is_case_insensitive() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::test_env::set_var(EXPANSION_MODE_ENV, "HyDE");
        assert!(enabled());
        crate::test_env::set_var(EXPANSION_MODE_ENV, "off");
        assert!(!enabled());
        crate::test_env::remove_var(EXPANSION_MODE_ENV);
    }

    #[test]
    fn truncation_is_char_safe() {
        let text = "é".repeat(700);
        let truncated = truncate_chars(&text, HYDE_MAX_CHARS);
        assert_eq!(truncated.chars().count(), HYDE_MAX_CHARS);
        assert!(truncated.chars().all(|c| c == 'é'));
    }
}
