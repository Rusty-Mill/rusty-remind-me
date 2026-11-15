//! The [`WikiWriter`] that asks a local model to write wiki pages, over the
//! OpenAI-compatible `/v1/chat/completions` endpoint that llama.cpp, vLLM
//! and Ollama all serve (card #747).
//!
//! It reuses the embedder's hand-rolled HTTP client, so `http://` only: a
//! deployment that needs TLS puts a reverse proxy in front, as for Ollama.
//! Off unless both [`URL_ENV`] and [`MODEL_ENV`] are set.

use crate::db::Database;
use crate::embedder::post_json_within;
use crate::topic_seed::{NameReport, NameRequest, NamedTopic, TopicNamer};
use crate::wiki_compile::{
    run, AutoOptions, AutoReport, PageDraft, PageRequest, RunGuard, WikiWriter,
};
use crate::wiki_fs::Wiki;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

/// Base URL of the model server, `http://host:port`.
pub const URL_ENV: &str = "REMIND_ME_WIKI_LLM_URL";
/// Model name the server is asked for.
pub const MODEL_ENV: &str = "REMIND_ME_WIKI_LLM_MODEL";
/// Seconds one page may take to write. A local model on a small machine is slow.
pub const TIMEOUT_ENV: &str = "REMIND_ME_WIKI_LLM_TIMEOUT_SECS";
/// Tokens one page may use.
pub const MAX_TOKENS_ENV: &str = "REMIND_ME_WIKI_LLM_MAX_TOKENS";
/// Cosine, above 0 and at most 1, a source needs to join an existing page.
pub const MATCH_ENV: &str = "REMIND_ME_WIKI_MATCH";

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const DEFAULT_MAX_TOKENS: u32 = 4096;
const PATH: &str = "/v1/chat/completions";
const LABEL: &str = "wiki model";

const INSTRUCTIONS: &str = "You maintain one page of a personal wiki, following the schema above. \
Reply with only a JSON object: {\"title\": \"...\", \"content\": \"...\"}. \
`content` is the whole page in Markdown. Fold every source into it and cite each one by its id \
in backticks, for example (`mem_123`). When updating a page, keep its title, keep everything it \
says unless a source corrects it, and keep every heading, [[link]] and `backticked` detail it \
has. Add to the page; do not condense it.";

/// Writes pages through a chat-completions endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCompletionsWriter {
    url: String,
    model: String,
    timeout: Duration,
    max_tokens: u32,
}

impl ChatCompletionsWriter {
    /// The writer the environment configures: `Ok(None)` when it names no
    /// model server, an error when a setting is malformed.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// [`from_env`](Self::from_env) over any variable lookup, for tests.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let set = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let (Some(url), Some(model)) = (set(URL_ENV), set(MODEL_ENV)) else {
            return Ok(None);
        };
        let timeout = number(set(TIMEOUT_ENV), TIMEOUT_ENV, DEFAULT_TIMEOUT_SECS)?;
        let max_tokens = number(set(MAX_TOKENS_ENV), MAX_TOKENS_ENV, DEFAULT_MAX_TOKENS)?;
        Ok(Some(Self {
            url: url.trim().trim_end_matches('/').to_string(),
            model: model.trim().to_string(),
            timeout: Duration::from_secs(timeout),
            max_tokens,
        }))
    }

    /// The chat request for one page.
    fn body(&self, request: &PageRequest<'_>) -> Value {
        json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": format!("{}\n\n{INSTRUCTIONS}", request.schema)},
                {"role": "user", "content": prompt(request)},
            ],
        })
    }
}

impl WikiWriter for ChatCompletionsWriter {
    fn write_page(&self, request: &PageRequest<'_>) -> Result<PageDraft, String> {
        let body = self.body(request).to_string();
        let reply = post_json_within(&self.url, LABEL, PATH, &body, self.timeout)
            .map_err(|e| e.to_string())?;
        parse_draft(&message_content(&reply)?)
    }
}

const NAME_INSTRUCTIONS: &str = "You name one topic in a personal knowledge base's topic tree. \
Reply with only a JSON object: {\"name\": \"...\", \"description\": \"...\"}. \
`name` is a short noun phrase of one to four words in Title Case, such as \"Systems Engineering\" \
or \"Rust Async\", naming what the samples (or subtopics) have in common, never a sentence. \
`description` is one sentence saying what belongs under the topic.";

impl TopicNamer for ChatCompletionsWriter {
    fn name_topic(&self, request: &NameRequest) -> Result<NamedTopic, String> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": NAME_INSTRUCTIONS},
                {"role": "user", "content": name_prompt(request)},
            ],
        })
        .to_string();
        let reply = post_json_within(&self.url, LABEL, PATH, &body, self.timeout)
            .map_err(|e| e.to_string())?;
        parse_named(&message_content(&reply)?)
    }
}

/// The user message for naming one topic: its subtopics' names when it has
/// any, then its most central members.
fn name_prompt(request: &NameRequest) -> String {
    let mut text = String::new();
    if !request.children.is_empty() {
        text.push_str("Subtopics:\n");
        for child in &request.children {
            text.push_str(&format!("- {child}\n"));
        }
        text.push('\n');
    }
    text.push_str("Samples:\n");
    for sample in &request.samples {
        text.push_str(&format!("- {}\n", sample.replace('\n', " ")));
    }
    text.push_str("\n/no_think");
    text
}

#[derive(Deserialize)]
struct Named {
    name: String,
    #[serde(default)]
    description: String,
}

/// The `{name, description}` in a model's answer, parsed like a page.
fn parse_named(text: &str) -> Result<NamedTopic, String> {
    let text = text.trim();
    let named: Named = match serde_json::from_str(text) {
        Ok(named) => named,
        Err(_) => serde_json::from_str(embedded_object(text)?)
            .map_err(|e| format!("the {LABEL}'s answer is not {{name, description}} JSON: {e}"))?,
    };
    if named.name.trim().is_empty() {
        return Err(format!("the {LABEL} gave an empty topic name"));
    }
    Ok(NamedTopic {
        name: named.name,
        description: named.description,
    })
}

/// Name up to `max_nodes` nodes of seed run `run` with the configured model.
/// Holds the wiki compile's claim on the wiki root for the whole run, so
/// naming and a compile never share the model's single slot.
pub fn name_topics_with_configured_model(
    db: &Database,
    run: &str,
    max_nodes: usize,
) -> Result<NameReport, String> {
    let namer = ChatCompletionsWriter::from_env()?.ok_or_else(|| {
        format!("naming topics needs a local model: set {URL_ENV} and {MODEL_ENV}")
    })?;
    let wiki = Wiki::from_env();
    let _slot = RunGuard::take(wiki.root())?;
    let dir = crate::topic_seed::snapshot_dir(&db.store())?;
    let samples = |ids: &[String]| -> Result<Vec<String>, String> {
        let store = db.store();
        let memories = crate::db::memories::Memories::new(&store);
        let found = memories.get_many(ids).map_err(|e| e.to_string())?;
        Ok(found.into_iter().map(|m| m.content).collect())
    };
    crate::topic_seed::name_pending(&dir, run, &namer, &samples, max_nodes)
}

/// Compile pending memories with the configured model, grouping them with
/// the configured embedder when one answers. `options.match_threshold` is
/// replaced by [`MATCH_ENV`] when that is set.
pub fn compile_with_configured_model(
    wiki: &Wiki,
    db: &Database,
    options: &AutoOptions,
) -> Result<AutoReport, String> {
    let writer = ChatCompletionsWriter::from_env()?.ok_or_else(|| {
        format!("automatic compile needs a local model: set {URL_ENV} and {MODEL_ENV}")
    })?;
    let set = std::env::var(MATCH_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let options = AutoOptions {
        match_threshold: match_threshold(set, options.match_threshold)?,
        ..options.clone()
    };
    let embedder = crate::embedder::available_embedder();
    run(wiki, db, &writer, embedder.as_deref(), &options).map_err(|e| e.to_string())
}

/// [`MATCH_ENV`]'s value, or `default` when unset.
fn match_threshold(value: Option<String>, default: f32) -> Result<f32, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value.trim().parse::<f32>() {
        Ok(n) if n > 0.0 && n <= 1.0 => Ok(n),
        _ => Err(format!(
            "{MATCH_ENV} must be above 0 and at most 1, got {value:?}"
        )),
    }
}

/// A positive whole number from `value`, or `default` when unset.
fn number<T: std::str::FromStr + PartialOrd + Default>(
    value: Option<String>,
    name: &str,
    default: T,
) -> Result<T, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value.trim().parse::<T>() {
        Ok(n) if n > T::default() => Ok(n),
        _ => Err(format!(
            "{name} must be a positive whole number, got {value:?}"
        )),
    }
}

/// The user message: the page as it stands, then the sources.
fn prompt(request: &PageRequest<'_>) -> String {
    let mut text = match request.page {
        Some(page) => format!(
            "Update the page titled {:?}. It currently reads:\n\n{}\n\n",
            page.title, page.content
        ),
        None => "Write a new page with a short, specific title.\n\n".to_string(),
    };
    if let Some(why) = request.feedback {
        text.push_str(&format!(
            "Your last draft of this page was refused: {why}. Write it again, keeping \
             every heading, [[link]] and `backticked` detail the page already has.\n\n"
        ));
    }
    text.push_str("Sources:\n");
    for source in request.sources {
        text.push_str(&format!(
            "- `{}` ({}, {}): {}\n",
            source.id, source.category, source.created_at, source.content
        ));
    }
    // Qwen models skip their thinking pass on this; others read it as noise.
    text.push_str("\n/no_think");
    text
}

/// The assistant message's text from a chat-completions reply.
fn message_content(reply: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(reply).map_err(|e| format!("the {LABEL} sent bad JSON: {e}"))?;
    let choice = &value["choices"][0];
    if choice["finish_reason"] == "length" {
        return Err(format!(
            "the {LABEL} ran out of tokens; raise {MAX_TOKENS_ENV}"
        ));
    }
    choice["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("the {LABEL} reply has no message content"))
}

#[derive(Deserialize)]
struct Draft {
    title: String,
    content: String,
}

/// The page in a model's answer. A reply that is the JSON page is taken as
/// it is, whatever its content says. Otherwise a leading `<think>` block is
/// dropped and the JSON object from the first `{` to the last `}` is parsed,
/// so a fenced or chatty answer still parses.
fn parse_draft(text: &str) -> Result<PageDraft, String> {
    let text = text.trim();
    let draft: Draft = match serde_json::from_str(text) {
        Ok(draft) => draft,
        Err(_) => serde_json::from_str(embedded_object(text)?)
            .map_err(|e| format!("the {LABEL}'s page is not {{title, content}} JSON: {e}"))?,
    };
    Ok(PageDraft {
        title: draft.title.trim().to_string(),
        content: draft.content,
    })
}

/// The `{...}` in `text`, after a leading `<think>` block if there is one.
fn embedded_object(text: &str) -> Result<&str, String> {
    let text = match text
        .strip_prefix("<think>")
        .and_then(|rest| rest.split_once("</think>"))
    {
        Some((_, after)) => after,
        None => text,
    };
    match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => Ok(&text[start..=end]),
        _ => Err(format!("the {LABEL} answered without a JSON page")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::CreatedMemory;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn a_topic_name_parses_from_json_or_chatty_text_and_never_empty() {
        let plain = parse_named(r#"{"name": "Systems Engineering", "description": "d"}"#).unwrap();
        assert_eq!(plain.name, "Systems Engineering");
        let chatty =
            parse_named("<think>hm</think>Sure: {\"name\": \"MBSE\"} hope that helps").unwrap();
        assert_eq!(
            (chatty.name.as_str(), chatty.description.as_str()),
            ("MBSE", "")
        );
        assert!(parse_named(r#"{"name": "  "}"#).is_err());
        assert!(parse_named("no json here").is_err());
        let prompt = name_prompt(&NameRequest {
            children: vec!["MBSE".into()],
            samples: vec!["line one\nline two".into()],
        });
        assert!(prompt.contains("- MBSE\n") && prompt.contains("- line one line two\n"));
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn off_unless_url_and_model_are_both_set() {
        assert_eq!(ChatCompletionsWriter::from_vars(vars(&[])), Ok(None));
        let url_only = vars(&[(URL_ENV, "http://ai:8080")]);
        assert_eq!(ChatCompletionsWriter::from_vars(url_only), Ok(None));
        let writer = ChatCompletionsWriter::from_vars(vars(&[
            (URL_ENV, "http://ai:8080/"),
            (MODEL_ENV, "qwen"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(writer.url, "http://ai:8080");
        assert_eq!(writer.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(writer.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn a_malformed_number_is_an_error_not_a_default() {
        for bad in ["0", "-1", "ten"] {
            let got = ChatCompletionsWriter::from_vars(vars(&[
                (URL_ENV, "http://ai:8080"),
                (MODEL_ENV, "qwen"),
                (TIMEOUT_ENV, bad),
            ]));
            assert!(got.unwrap_err().contains(TIMEOUT_ENV), "{bad}");
        }
    }

    #[test]
    fn the_match_threshold_must_be_a_cosine() {
        assert_eq!(match_threshold(None, 0.55), Ok(0.55));
        assert_eq!(match_threshold(Some("0.7".into()), 0.55), Ok(0.7));
        for bad in ["0", "1.5", "high"] {
            assert!(match_threshold(Some(bad.into()), 0.55).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_draft_finds_the_object_in_a_chatty_answer() {
        let answer = "<think>{\"x\":1}</think>Sure!\n```json\n{\"title\": \" Rust \", \"content\": \"A {braced} page\"}\n```";
        let draft = parse_draft(answer).unwrap();
        assert_eq!(draft.title, "Rust");
        assert_eq!(draft.content, "A {braced} page");
        assert!(parse_draft("no page here").is_err());
        // The literal tag inside a valid page is content, not a wrapper.
        let about_tags = r#"{"title": "Tags", "content": "Models end reasoning with </think>."}"#;
        assert_eq!(
            parse_draft(about_tags).unwrap().content,
            "Models end reasoning with </think>."
        );
        let wrapped = format!("<think>plan</think>\n{about_tags}");
        assert_eq!(
            parse_draft(&wrapped).unwrap().content,
            "Models end reasoning with </think>."
        );
        assert!(parse_draft("{\"title\": \"Rust\"}").is_err());
    }

    #[test]
    fn a_truncated_reply_names_the_setting_to_raise() {
        let reply = r#"{"choices":[{"finish_reason":"length","message":{"content":"{"}}]}"#;
        assert!(message_content(reply).unwrap_err().contains(MAX_TOKENS_ENV));
    }

    /// One canned HTTP reply on a loopback port; returns the URL and a
    /// handle yielding the raw request the writer sent.
    fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = stream.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&seen);
                if let Some(split) = text.find("\r\n\r\n") {
                    let length: usize = text[..split]
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if seen.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8(seen).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn a_trickling_server_cannot_outlast_the_timeout() {
        // One byte every 200ms never goes idle long enough for a socket
        // timeout; only a deadline on the whole call stops it.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n");
            while stream.write_all(b"x").is_ok() {
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let writer = ChatCompletionsWriter::from_vars(vars(&[
            (URL_ENV, &url),
            (MODEL_ENV, "qwen"),
            (TIMEOUT_ENV, "1"),
        ]))
        .unwrap()
        .unwrap();
        let started = std::time::Instant::now();

        let got = writer.write_page(&PageRequest {
            schema: "",
            page: None,
            sources: &[],
            feedback: None,
        });

        assert!(got.unwrap_err().contains("reading from"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn write_page_posts_the_sources_and_reads_back_the_page() {
        let (url, server) = serve_once(
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"{\"title\":\"Rust\",\"content\":\"Rust (`mem_1`)\"}"}}]}"#,
        );
        let writer =
            ChatCompletionsWriter::from_vars(vars(&[(URL_ENV, &url), (MODEL_ENV, "qwen")]))
                .unwrap()
                .unwrap();
        let sources = [CreatedMemory {
            id: "mem_1".into(),
            category: "fact".into(),
            content: "Rust has ownership".into(),
            created_at: "2026-11-05".into(),
        }];
        let draft = writer
            .write_page(&PageRequest {
                schema: "SCHEMA",
                page: None,
                sources: &sources,
                feedback: Some("it dropped `9001`"),
            })
            .unwrap();
        assert_eq!(draft.title, "Rust");
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(sent.contains("\"model\":\"qwen\""));
        assert!(sent.contains("`mem_1`"));
        assert!(sent.contains("SCHEMA"));
        assert!(sent.contains("was refused: it dropped `9001`"));
    }
}
