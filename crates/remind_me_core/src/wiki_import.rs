//! Import loose Markdown files into `wiki_pages`.
//!
//! Written for the `daily-backup-system` (dbs) `wiki` export — `dbs
//! export-wiki --out-dir DIR` drops one `.md` per page plus an `index.md`
//! into a directory — but nothing here is dbs-specific: any Markdown file
//! imports, with sensible fallbacks when it carries no front matter.
//!
//! ## Front matter contract
//!
//! `wiki_pages` needs `slug`, `title` and `topic`, and a plain Markdown file
//! only implies the second one. dbs therefore emits all three explicitly in
//! YAML front matter:
//!
//! ```text
//! ---
//! slug: "source-raindrop"
//! title: "Source: raindrop"
//! topic: "source"
//! ---
//!
//! # Source: raindrop
//! ...
//! ```
//!
//! Each field falls back independently when absent, so a hand-written note
//! still imports: `title` from the first `# ` heading then the file stem,
//! `slug` derived from the resolved title, `topic` to `"general"` (matching
//! the `wiki-write` CLI default).
//!
//! The stored `content` is the body *after* the front matter. The three
//! fields it carries become columns, so keeping them in the body too would
//! store them twice and surface YAML to a reader of the page.
//!
//! Importing is idempotent: `write_wiki_page` upserts on `slug`, and dbs's
//! slugs are stable across exports, so re-importing a re-exported directory
//! updates pages in place instead of duplicating them.

use rusqlite::Connection;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use crate::wiki::write_wiki_page;

/// One page that was written to `wiki_pages`.
#[derive(Debug, Clone)]
pub struct ImportedPage {
    pub path: String,
    pub slug: String,
    pub title: String,
    pub topic: String,
}

/// Outcome of an import run.
#[derive(Debug, Default)]
pub struct WikiImportReport {
    pub imported: Vec<ImportedPage>,
    /// `(path, reason)` for files found but not imported (e.g. unreadable).
    pub skipped: Vec<(String, String)>,
}

/// Lowercase kebab slug. Mirrors dbs's `dbs.export.wiki.slugify` so a page
/// whose front matter lost its `slug` still lands on the same identity.
pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "page".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Undo the double-quoted YAML scalar dbs writes (only `\\` and `\"` are
/// ever escaped there). An unquoted value is returned as-is.
fn unquote(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some(next) => out.push(next),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        v.to_string()
    }
}

fn is_newline(c: char) -> bool {
    c == '\r' || c == '\n'
}

/// Split leading `---` front matter from the body.
///
/// Returns empty fields and the untouched text when the file has no front
/// matter, or when a `---` opener is never closed — an unterminated block is
/// far more likely to be prose containing a rule than truncated metadata, so
/// nothing is silently swallowed.
pub fn parse_front_matter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::new();
    let rest = match text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None => return (fields, text),
    };

    let mut offset = 0usize;
    let mut body_start = None;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(is_newline);
        if trimmed.trim_end() == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                fields.insert(key.to_string(), unquote(value));
            }
        }
        offset += line.len();
    }

    match body_start {
        Some(start) => (fields, rest[start..].trim_start_matches(is_newline)),
        None => (HashMap::new(), text),
    }
}

struct ParsedPage {
    slug: String,
    title: String,
    /// dbs front-matter topic. Reported back to the caller but **not stored**:
    /// `wiki_pages` has no `topic` column once the schema is generated from
    /// `remind_me`. Kept because it is genuine metadata about the import.
    topic: String,
    /// dbs front-matter summary, which does map onto `wiki_pages.summary`.
    summary: String,
    content: String,
}

fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .find(|line| line.starts_with("# "))
        .map(|line| line[2..].trim().to_string())
        .filter(|title| !title.is_empty())
}

fn parse_page(path: &Path, text: &str) -> ParsedPage {
    let (fields, body) = parse_front_matter(text);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "page".to_string());

    let field = |key: &str| -> Option<String> {
        fields
            .get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    let title = field("title")
        .or_else(|| first_heading(body))
        .unwrap_or_else(|| stem.clone());
    let slug = field("slug").unwrap_or_else(|| slugify(&title));
    let topic = field("topic").unwrap_or_else(|| "general".to_string());
    let summary = field("summary").unwrap_or_default();

    ParsedPage {
        slug,
        title,
        topic,
        summary,
        content: body.to_string(),
    }
}

fn collect_markdown(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_markdown(&path, recursive, out)?;
            }
            continue;
        }
        let is_markdown = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false);
        if is_markdown {
            out.push(path);
        }
    }
    Ok(())
}

/// Import every Markdown file under `dir` into `wiki_pages`.
///
/// Files are imported in sorted path order so a run is reproducible. A file
/// that cannot be read is recorded in `skipped` and does not abort the run;
/// a database error does abort, since that means no further write will work
/// either.
pub fn import_wiki_dir(
    conn: &Connection,
    dir: &Path,
    recursive: bool,
) -> Result<WikiImportReport, Box<dyn Error>> {
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()).into());
    }

    let mut files = Vec::new();
    collect_markdown(dir, recursive, &mut files)?;
    files.sort();

    let mut report = WikiImportReport::default();
    for path in files {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                report
                    .skipped
                    .push((path.display().to_string(), err.to_string()));
                continue;
            }
        };
        let page = parse_page(&path, &text);
        write_wiki_page(conn, &page.slug, &page.title, &page.content, &page.summary)?;
        report.imported.push(ImportedPage {
            path: path.display().to_string(),
            slug: page.slug,
            title: page.title,
            topic: page.topic,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn slugify_matches_dbs_rules() {
        assert_eq!(slugify("Source: raindrop"), "source-raindrop");
        assert_eq!(slugify("Tag: rust"), "tag-rust");
        assert_eq!(slugify("  --- "), "page");
        assert_eq!(slugify("Tokio: an async runtime"), "tokio-an-async-runtime");
    }

    #[test]
    fn parses_dbs_front_matter_and_strips_it_from_the_body() {
        let text = "---\nslug: \"source-rd\"\ntitle: \"Source: rd\"\ntopic: \"source\"\n---\n\n# Source: rd\n\nbody\n";
        let (fields, body) = parse_front_matter(text);
        assert_eq!(fields.get("slug").unwrap(), "source-rd");
        // The value's own colon must survive the key/value split.
        assert_eq!(fields.get("title").unwrap(), "Source: rd");
        assert_eq!(fields.get("topic").unwrap(), "source");
        assert!(body.starts_with("# Source: rd"));
        assert!(!body.contains("slug:"));
    }

    #[test]
    fn unescapes_quoted_scalars() {
        let text = "---\ntitle: \"Title: \\\"quoted\\\" & tricky\"\n---\nbody\n";
        let (fields, _) = parse_front_matter(text);
        assert_eq!(fields.get("title").unwrap(), "Title: \"quoted\" & tricky");
    }

    #[test]
    fn file_without_front_matter_keeps_its_whole_body() {
        let text = "# Just A Note\n\nsome prose\n";
        let (fields, body) = parse_front_matter(text);
        assert!(fields.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn unterminated_front_matter_is_treated_as_body() {
        let text = "---\nslug: \"x\"\nstill going\n";
        let (fields, body) = parse_front_matter(text);
        assert!(fields.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn falls_back_to_heading_then_stem() {
        let page = parse_page(&PathBuf::from("/w/my-note.md"), "# Real Title\n\nbody\n");
        assert_eq!(page.title, "Real Title");
        assert_eq!(page.slug, "real-title");
        assert_eq!(page.topic, "general");

        let page = parse_page(&PathBuf::from("/w/my-note.md"), "no heading here\n");
        assert_eq!(page.title, "my-note");
        assert_eq!(page.slug, "my-note");
    }
}
