//! Rule-based extraction of references and entities from memory text.
//!
//! Pure and hand-rolled (this crate has no `regex` dependency, and the rules
//! are small enough not to need one). It runs at write time so "everything
//! about rusty_mill#321" is a lookup rather than a search. The rules are
//! deliberately conservative: a missed reference costs little, a spurious one
//! pollutes the reverse index, so ambiguous shapes (a bare `#123`, a plain
//! number, an email) are never extracted.

use crate::models::EntityInput;
use std::collections::HashSet;

/// At most this many references per memory.
pub const MAX_REFERENCES: usize = 50;
/// At most this many entities per memory.
pub const MAX_ENTITIES: usize = 20;
const MIN_IDENT_LEN: usize = 7;
const MAX_RUN_WORDS: usize = 4;

/// One extracted reference, before it is bound to a memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReferenceParts {
    /// `url`, `issue`, `pull`, `commit`, `path` or `handle`.
    pub kind: &'static str,
    pub value: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    pub references: Vec<NewReferenceParts>,
    pub entities: Vec<EntityInput>,
}

/// Whether extraction is on: `REMIND_ME_EXTRACT=0` turns it off globally.
pub fn enabled_by_env() -> bool {
    !matches!(std::env::var("REMIND_ME_EXTRACT").as_deref(), Ok("0"))
}

/// Extract references and entities from `content`.
pub fn extract(content: &str) -> Extracted {
    Extracted {
        references: extract_references(content),
        entities: extract_entities(content),
    }
}

fn trim_token(token: &str) -> &str {
    let token = token.trim_start_matches(['(', '[', '<', '"', '\'', '`', '*']);
    token.trim_end_matches([
        '.', ',', ';', ':', ')', ']', '>', '"', '\'', '`', '!', '?', '*',
    ])
}

fn extract_references(content: &str) -> Vec<NewReferenceParts> {
    let mut out: Vec<NewReferenceParts> = Vec::new();
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    for raw in content.split_whitespace() {
        if out.len() >= MAX_REFERENCES {
            break;
        }
        let Some(found) = reference_of(trim_token(raw)) else {
            continue;
        };
        if seen.insert((found.kind, found.value.clone())) {
            out.push(found);
        }
    }
    out
}

fn part(kind: &'static str, value: impl Into<String>, label: Option<String>) -> NewReferenceParts {
    NewReferenceParts {
        kind,
        value: value.into(),
        label,
    }
}

fn reference_of(token: &str) -> Option<NewReferenceParts> {
    if token.is_empty() {
        return None;
    }
    if token.starts_with("http://") || token.starts_with("https://") {
        return Some(url_reference(token));
    }
    if let Some(value) = repo_issue(token) {
        return Some(part("issue", value, None));
    }
    if is_commit(token) {
        return Some(part("commit", token, None));
    }
    if let Some(handle) = handle_of(token) {
        return Some(part("handle", handle, None));
    }
    path_reference(token)
}

/// `https://github.com/o/r/issues/5` becomes issue `o/r#5` (`pull` for
/// `/pull/`); any other URL is kept whole.
fn url_reference(url: &str) -> NewReferenceParts {
    match github_item(url) {
        Some((kind, value)) => part(kind, value, None),
        None => part("url", url, None),
    }
}

fn github_item(url: &str) -> Option<(&'static str, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [owner, repo, kind, number, ..] = parts.as_slice() else {
        return None;
    };
    let number = number.split(['#', '?']).next().unwrap_or("");
    let kind = match *kind {
        "issues" => "issue",
        "pull" => "pull",
        _ => return None,
    };
    let numeric = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    if !numeric || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((kind, format!("{owner}/{repo}#{number}")))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// `owner/repo#123` and nothing looser.
fn repo_issue(token: &str) -> Option<String> {
    let (repo_part, number) = token.split_once('#')?;
    let (owner, repo) = repo_part.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && s.chars().all(is_name_char);
    let numeric = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    (ok(owner) && ok(repo) && numeric).then(|| token.to_string())
}

/// 7-40 lowercase hex with at least one digit and one letter a-f, so plain
/// numbers and words like `defaced` are not commits.
fn is_commit(token: &str) -> bool {
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    (7..=40).contains(&token.len())
        && token.bytes().all(hex)
        && token.bytes().any(|b| b.is_ascii_digit())
        && token.bytes().any(|b| b.is_ascii_lowercase())
}

fn handle_of(token: &str) -> Option<String> {
    let name = token.strip_prefix('@')?;
    let valid = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    valid.then(|| token.to_string())
}

/// A file extension: short, alphanumeric, starting with a letter.
fn has_extension(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=8).contains(&ext.len())
        && ext.starts_with(|c: char| c.is_ascii_alphabetic())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

fn path_reference(token: &str) -> Option<NewReferenceParts> {
    let (path, line) = match token.rsplit_once(':') {
        Some((p, l)) if !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()) => (p, Some(l)),
        _ => (token, None),
    };
    let path_chars = path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-' | '~'));
    if path.is_empty() || !path_chars || path.contains("//") {
        return None;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if !has_extension(file) {
        return None;
    }
    // Without a slash only the `file.rs:123` form is unambiguous enough
    // ("e.g", "v1.2" and the like are not paths).
    if !path.contains('/') && line.is_none() {
        return None;
    }
    Some(part("path", path, line.map(|_| token.to_string())))
}

fn is_sentence_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | ':' | ';' | ',' | ')')
}

fn is_capitalised(word: &str) -> bool {
    word.chars().count() >= 2
        && word.starts_with(|c: char| c.is_uppercase())
        && word.chars().all(char::is_alphabetic)
}

/// Words that open a sentence and are capitalised only for that reason.
const LEADING: &[&str] = &[
    "The", "A", "An", "This", "That", "These", "Those", "We", "It", "My", "Our", "Then", "Also",
    "But", "And", "So", "If", "When", "After", "Before", "In", "On", "At", "For", "To", "Of",
    "Today", "There", "Here", "He", "She", "They", "You", "Your", "Its", "As", "By", "With",
];

fn extract_entities(content: &str) -> Vec<EntityInput> {
    let mut names = capitalised_runs(content);
    names.extend(repeated_identifiers(content));
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.to_lowercase()));
    names.truncate(MAX_ENTITIES);
    names
        .into_iter()
        .map(|name| EntityInput {
            name,
            kind: None,
            aliases: Vec::new(),
        })
        .collect()
}

fn flush_run(run: &mut Vec<String>, out: &mut Vec<String>) {
    let skip = usize::from(run.first().is_some_and(|w| LEADING.contains(&w.as_str())));
    let words = &run[skip.min(run.len())..];
    if (2..=MAX_RUN_WORDS).contains(&words.len()) {
        out.push(words.join(" "));
    }
    run.clear();
}

/// Two-to-four-word runs of capitalised words. A single capitalised word is
/// never an entity: it is usually just the start of a sentence.
fn capitalised_runs(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for raw in content.split_whitespace() {
        let word = raw.trim_start_matches(['(', '[', '"', '\'', '*', '`']);
        let ends = word.ends_with(is_sentence_end);
        let word = word.trim_end_matches(|c: char| !c.is_alphanumeric());
        if is_capitalised(word) {
            run.push(word.to_string());
            if ends {
                flush_run(&mut run, &mut out);
            }
        } else {
            flush_run(&mut run, &mut out);
        }
    }
    flush_run(&mut run, &mut out);
    out
}

fn is_snake(w: &str) -> bool {
    w.contains('_')
        && !w.starts_with('_')
        && !w.ends_with('_')
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && w.chars().any(|c| c.is_ascii_lowercase())
}

fn is_pascal(w: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_uppercase())
        && w.chars().all(|c| c.is_ascii_alphanumeric())
        && w.chars().skip(1).any(|c| c.is_ascii_uppercase())
        && w.chars().any(|c| c.is_ascii_lowercase())
}

/// `snake_case` / `PascalCase` identifiers over six characters that occur at
/// least twice: once is a passing word, twice is a subject.
fn repeated_identifiers(content: &str) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for raw in content.split_whitespace() {
        let word = trim_token(raw);
        if word.len() < MIN_IDENT_LEN || !(is_snake(word) || is_pascal(word)) {
            continue;
        }
        let count = counts.entry(word.to_string()).or_insert(0);
        if *count == 0 {
            order.push(word.to_string());
        }
        *count += 1;
    }
    order.into_iter().filter(|w| counts[w] >= 2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    type Refs = Vec<(&'static str, String)>;

    fn refs(text: &str) -> Refs {
        extract(text)
            .references
            .into_iter()
            .map(|r| (r.kind, r.value))
            .collect()
    }

    fn one(kind: &'static str, value: &str) -> Refs {
        vec![(kind, value.to_string())]
    }

    #[test]
    fn reference_rules_table() {
        let cases: Vec<(&str, Refs)> = vec![
            ("see rusty_mill/x#321 now", one("issue", "rusty_mill/x#321")),
            (
                "see Rusty-Mill/rusty_mill#321.",
                one("issue", "Rusty-Mill/rusty_mill#321"),
            ),
            ("https://github.com/o/r/issues/5", one("issue", "o/r#5")),
            ("(https://github.com/o/r/pull/7).", one("pull", "o/r#7")),
            (
                "https://github.com/o/r/tree/main",
                one("url", "https://github.com/o/r/tree/main"),
            ),
            (
                "go to https://example.org/a?b=1, ok",
                one("url", "https://example.org/a?b=1"),
            ),
            ("fixed in 3ac9f797 today", one("commit", "3ac9f797")),
            ("src/foo.rs", one("path", "src/foo.rs")),
            ("/etc/x.conf", one("path", "/etc/x.conf")),
            ("./a/b.py,", one("path", "./a/b.py")),
            ("@baileyrd please look", one("handle", "@baileyrd")),
        ];
        for (text, want) in cases {
            assert_eq!(refs(text), want, "{text}");
        }
    }

    #[test]
    fn reference_negatives() {
        for text in [
            "issue #123 is open",
            "rusty_mill#321 has no owner",
            "1234567 items",
            "deadbee has no digit",
            "defaced",
            "email me at bob@example.com",
            "see e.g. this",
            "version 1.2 shipped",
            "and/or maybe",
            "1/2.5",
            "a//b.rs",
            "@",
        ] {
            assert_eq!(refs(text), Refs::new(), "{text}");
        }
    }

    #[test]
    fn path_with_line_keeps_the_line_in_the_label() {
        let r = extract("crash at main.rs:42 and src/lib.rs:7:").references;
        assert_eq!(r[0].value, "main.rs");
        assert_eq!(r[0].label.as_deref(), Some("main.rs:42"));
        assert_eq!(r[1].value, "src/lib.rs");
        assert_eq!(r[1].label.as_deref(), Some("src/lib.rs:7"));
    }

    #[test]
    fn references_are_deduped_and_capped() {
        assert_eq!(refs("a/b#1 a/b#1 https://github.com/a/b/issues/1").len(), 1);
        let many: String = (0..80).map(|i| format!("o/r#{i} ")).collect();
        assert_eq!(extract(&many).references.len(), MAX_REFERENCES);
    }

    fn entities(text: &str) -> Vec<String> {
        extract(text).entities.into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn capitalised_runs_are_entities() {
        assert_eq!(entities("We use Rusty Mill daily."), vec!["Rusty Mill"]);
        assert_eq!(
            entities("The Daily Backup System failed, so Rusty Mill paged."),
            vec!["Daily Backup System", "Rusty Mill"]
        );
    }

    #[test]
    fn entity_negatives() {
        for text in [
            "Today we shipped it.",
            "Rusty is a crate.",
            "Done. Shipped. Merged.",
            "One Two Three Four Five words",
            "i use rust",
        ] {
            assert_eq!(entities(text), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn identifiers_need_two_occurrences_and_length() {
        assert_eq!(
            entities("call apply_entity_mentions then apply_entity_mentions again"),
            vec!["apply_entity_mentions"]
        );
        assert!(entities("call apply_entity_mentions once").is_empty());
        assert_eq!(
            entities("use MemoryStore and MemoryStore"),
            vec!["MemoryStore"]
        );
        assert!(entities("a_b a_b").is_empty());
    }

    #[test]
    fn entities_are_capped() {
        let text: String = (0..40)
            .map(|i| format!("Alpha{} Beta{}, ", "x".repeat(i), "y".repeat(i)))
            .collect();
        assert_eq!(entities(&text).len(), MAX_ENTITIES);
    }
}
