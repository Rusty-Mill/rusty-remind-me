//! The wiki's on-disk layer: **files are the source of truth**.
//!
//! `wiki_pages`, `wiki_fts` and `wiki_links` are a cache rebuilt from the
//! directory, not a store in their own right. That inversion is the whole point
//! of the design: a wiki you can edit in any text editor, keep in git, and read
//! without the server running.
//!
//! # One writer
//!
//! [`Wiki::reconcile`] is the only path from a file into `wiki_pages`.
//! [`Wiki::write_page`] writes the file and then indexes it through the same
//! function, so there is no second route by which the index and the disk can
//! disagree. Every read path reconciles first, which is cheap — a `stat` per
//! file, re-indexing only what changed.
//!
//! # Generated pages
//!
//! `index.md`, `log.md` and `schema.md` are system pages: regenerated or
//! appended by this module, excluded from the page listing, and refused by
//! delete. They are on disk so the directory is self-describing to a human
//! reading it without any tooling.

use crate::wiki::{WikiDeleteOutcome, WikiPage, WikiSearchHit, RESERVED_SLUGS};
use crate::wiki_import::slugify;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Environment variable naming the wiki directory.
pub const WIKI_DIR_ENV: &str = "REMIND_ME_WIKI_DIR";

pub const INDEX_FILE: &str = "index.md";
pub const LOG_FILE: &str = "log.md";
pub const SCHEMA_FILE: &str = "schema.md";

/// `wiki_meta` key holding the compile watermark: the `created_at` of the last
/// raw memory folded into the wiki.
pub const COMPILE_WATERMARK_KEY: &str = "last_compile_at";

/// Earliest timestamp, used when no compile has run.
const EPOCH: &str = "1970-01-01T00:00:00+00:00";

/// Characters of a raw memory shown in a compile brief.
const COMPILE_SOURCE_CHARS: usize = 2_000;

/// What a reconcile pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileStats {
    /// Files newly indexed or re-indexed because they changed.
    pub indexed: usize,
    /// Index rows dropped because their file is gone.
    pub removed: usize,
    /// Content pages on disk after the pass.
    pub pages: usize,
}

/// Result of writing a page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiWriteOutcome {
    pub slug: String,
    pub title: String,
    pub created: bool,
    pub path: String,
    /// Titles this page links to via `[[wikilink]]`.
    pub links: Vec<String>,
}

/// The whole wiki, concatenated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiLoad {
    pub content: String,
    pub pages_included: usize,
    pub pages_omitted: usize,
    pub estimated_tokens: usize,
}

/// A compile brief, or the result of advancing the watermark.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WikiCompile {
    /// Phase one: what to synthesise. Does **not** advance the watermark.
    Brief {
        pending: usize,
        watermark: String,
        brief: String,
    },
    /// Phase two: the watermark moved past the surfaced batch.
    Integrated {
        sources_marked: usize,
        watermark: String,
    },
    /// Nothing pending — reported rather than silently bumping anything.
    Noop { reason: String, watermark: String },
}

/// Rough token count. Four characters per token is the reference's estimate.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// The wiki rooted at a directory.
#[derive(Debug, Clone)]
pub struct Wiki {
    root: PathBuf,
}

impl Wiki {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured wiki directory.
    ///
    /// `REMIND_ME_WIKI_DIR`, or `~/.remind-me/wiki` (falling back to
    /// `~/.remind_me/wiki` — underscore — if that's the only one that
    /// exists; see `db::resolve_memory_dir_child`, #228). Read at call time
    /// rather than cached so a caller can relocate it without restarting.
    pub fn from_env() -> Self {
        let root = std::env::var(WIKI_DIR_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::db::resolve_memory_dir_child("wiki"));
        Self::new(root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(io_error)
    }

    /// On-disk path for a page, addressed by title or slug.
    pub fn page_path(&self, title_or_slug: &str) -> PathBuf {
        self.root.join(format!("{}.md", slugify(title_or_slug)))
    }

    /// Every content page on disk, keyed by slug. System pages excluded.
    fn page_files(&self) -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut files: Vec<(String, PathBuf)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .filter(|p| {
                !matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some(INDEX_FILE) | Some(LOG_FILE) | Some(SCHEMA_FILE)
                )
            })
            .filter_map(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| (s.to_lowercase(), p.clone()))
            })
            .collect();
        files.sort();
        files
    }

    /// Index one file into `wiki_pages` and `wiki_links`.
    ///
    /// The sole writer of those tables from disk — [`Wiki::write_page`] routes
    /// through here too, so the index cannot diverge from the file by taking a
    /// different path in.
    fn index_page(&self, conn: &Connection, slug: &str, path: &Path) -> Result<()> {
        let content = std::fs::read_to_string(path).map_err(io_error)?;
        let title = extract_title(&content, slug);
        let summary = extract_summary(&content);
        let mtime = mtime_of(path);

        conn.execute(
            "INSERT INTO wiki_pages (slug, title, content, summary, mtime, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(slug) DO UPDATE SET
                title = excluded.title, content = excluded.content,
                summary = excluded.summary, mtime = excluded.mtime,
                updated_at = excluded.updated_at",
            params![
                slug,
                title,
                content,
                summary,
                mtime,
                Utc::now().to_rfc3339()
            ],
        )?;

        // Links are replaced wholesale: an edit that removes a `[[link]]` must
        // remove the edge, which an insert-only pass would leave behind.
        conn.execute("DELETE FROM wiki_links WHERE src_slug = ?", params![slug])?;
        for (dst_slug, dst_title) in parse_wikilinks(&content) {
            conn.execute(
                "INSERT OR IGNORE INTO wiki_links (src_slug, dst_slug, dst_title)
                 VALUES (?, ?, ?)",
                params![slug, dst_slug, dst_title],
            )?;
        }
        Ok(())
    }

    /// Bring the index in line with the directory.
    ///
    /// Re-indexes files whose modification time differs from the cached value,
    /// and drops rows whose file is gone. Cheap enough to run at the head of
    /// every read path, which is what keeps an out-of-band edit — someone
    /// changing a page in their editor — visible without any explicit sync.
    pub fn reconcile(&self, conn: &Connection) -> Result<ReconcileStats> {
        self.ensure_root()?;
        let files = self.page_files();

        let mut stmt = conn.prepare("SELECT slug, mtime FROM wiki_pages")?;
        let cached: std::collections::HashMap<String, f64> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?
            .collect::<Result<_>>()?;
        drop(stmt);

        let mut stats = ReconcileStats {
            pages: files.len(),
            ..Default::default()
        };

        for (slug, path) in &files {
            let mtime = mtime_of(path);
            // Compared exactly: a file rewritten with identical content still
            // has a new mtime and is re-indexed, which is cheaper than hashing
            // every file on every read.
            if cached.get(slug).map(|m| *m != mtime).unwrap_or(true) {
                self.index_page(conn, slug, path)?;
                stats.indexed += 1;
            }
        }

        let on_disk: std::collections::HashSet<&String> =
            files.iter().map(|(slug, _)| slug).collect();
        for slug in cached.keys().filter(|s| !on_disk.contains(s)) {
            conn.execute("DELETE FROM wiki_pages WHERE slug = ?", params![slug])?;
            conn.execute("DELETE FROM wiki_links WHERE src_slug = ?", params![slug])?;
            stats.removed += 1;
        }

        Ok(stats)
    }

    /// Create or replace a page.
    ///
    /// The H1 title is normalised onto the content so the file is
    /// self-describing — someone opening it in an editor sees what it is
    /// without consulting the index.
    ///
    /// Refuses [`RESERVED_SLUGS`]: those are generated, and letting a caller
    /// overwrite `index.md` by hand would put the index permanently at odds
    /// with the pages it claims to list.
    pub fn write_page(
        &self,
        conn: &Connection,
        title: &str,
        content: &str,
        log_note: Option<&str>,
    ) -> Result<std::result::Result<WikiWriteOutcome, WikiDeleteOutcome>> {
        let slug = slugify(title);
        if RESERVED_SLUGS.contains(&slug.as_str()) {
            return Ok(Err(WikiDeleteOutcome::Reserved));
        }
        self.ensure_root()?;

        let body = normalise_heading(title, content);
        let path = self.page_path(&slug);
        let created = !path.exists();
        std::fs::write(&path, &body).map_err(io_error)?;

        self.index_page(conn, &slug, &path)?;
        self.rebuild_index(conn)?;
        self.append_log(&format!(
            "{} [[{}]]",
            if created { "created" } else { "updated" },
            title
        ))?;
        if let Some(note) = log_note.filter(|n| !n.trim().is_empty()) {
            self.append_log(&format!("  note: {}", note))?;
        }

        Ok(Ok(WikiWriteOutcome {
            slug,
            title: title.to_string(),
            created,
            path: path.display().to_string(),
            links: parse_wikilinks(&body).into_iter().map(|(_, t)| t).collect(),
        }))
    }

    /// Read a page by title or slug, reconciling first.
    pub fn read_page(&self, conn: &Connection, title_or_slug: &str) -> Result<Option<WikiPage>> {
        self.reconcile(conn)?;
        crate::wiki::get_wiki_page(conn, &slugify(title_or_slug))
    }

    /// Every content page, most recently revised first.
    pub fn list_pages(&self, conn: &Connection) -> Result<Vec<WikiPage>> {
        self.reconcile(conn)?;
        crate::wiki::list_wiki_pages(conn)
    }

    /// Full-text search over the reconciled index.
    pub fn search_pages(
        &self,
        conn: &Connection,
        query: &str,
        limit: usize,
    ) -> Result<Vec<WikiSearchHit>> {
        self.reconcile(conn)?;
        crate::wiki::search_wiki_pages(conn, query, limit)
    }

    /// Delete a page's file and its index rows.
    pub fn delete_page(&self, conn: &Connection, title_or_slug: &str) -> Result<WikiDeleteOutcome> {
        let slug = slugify(title_or_slug);
        if RESERVED_SLUGS.contains(&slug.as_str()) {
            return Ok(WikiDeleteOutcome::Reserved);
        }
        self.reconcile(conn)?;

        let path = self.page_path(&slug);
        if !path.exists() {
            return Ok(WikiDeleteOutcome::NotFound);
        }
        std::fs::remove_file(&path).map_err(io_error)?;
        conn.execute("DELETE FROM wiki_pages WHERE slug = ?", params![slug])?;
        conn.execute("DELETE FROM wiki_links WHERE src_slug = ?", params![slug])?;

        self.rebuild_index(conn)?;
        self.append_log(&format!("deleted [[{}]]", title_or_slug))?;
        Ok(WikiDeleteOutcome::Deleted)
    }

    /// Regenerate `index.md` from the current pages.
    pub fn rebuild_index(&self, conn: &Connection) -> Result<String> {
        let mut stmt =
            conn.prepare("SELECT title, summary FROM wiki_pages ORDER BY title COLLATE NOCASE")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_>>()?;
        drop(stmt);

        let mut lines = vec![
            "# Wiki Index".to_string(),
            String::new(),
            format!(
                "_Auto-generated by rusty_remind_me — {} page(s). Do not edit by hand._",
                rows.len()
            ),
            String::new(),
        ];
        if rows.is_empty() {
            lines.push("_(empty)_".to_string());
        } else {
            for (title, summary) in rows {
                let tail = if summary.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", summary)
                };
                lines.push(format!("- [[{}]]{}", title, tail));
            }
        }
        let content = format!("{}\n", lines.join("\n"));
        self.ensure_root()?;
        std::fs::write(self.root.join(INDEX_FILE), &content).map_err(io_error)?;
        Ok(content)
    }

    /// Append a timestamped line to the append-only change log.
    pub fn append_log(&self, note: &str) -> Result<()> {
        use std::io::Write;
        self.ensure_root()?;
        let path = self.root.join(LOG_FILE);
        if !path.exists() {
            std::fs::write(&path, "# Wiki Change Log\n\n").map_err(io_error)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        writeln!(file, "- {} {}", Utc::now().to_rfc3339(), note).map_err(io_error)
    }

    /// The maintainer schema, written on first use so it can be edited.
    pub fn read_schema(&self) -> Result<String> {
        self.ensure_root()?;
        let path = self.root.join(SCHEMA_FILE);
        if !path.exists() {
            std::fs::write(&path, DEFAULT_SCHEMA).map_err(io_error)?;
        }
        std::fs::read_to_string(&path).map_err(io_error)
    }

    /// Concatenate the whole wiki for direct loading into context.
    ///
    /// This is the point of having a wiki at all: instead of retrieving
    /// fragments, load the synthesised, cross-linked whole. Pages come
    /// newest-revised first until the budget is spent.
    ///
    /// **Overflow is listed by title, not silently dropped** — a caller has to
    /// be able to tell that it received part of the wiki, and what to fetch
    /// individually.
    pub fn load(
        &self,
        conn: &Connection,
        token_budget: usize,
        include_index: bool,
    ) -> Result<WikiLoad> {
        self.reconcile(conn)?;

        let mut stmt = conn.prepare(
            "SELECT title, content, summary FROM wiki_pages
              ORDER BY updated_at DESC, title COLLATE NOCASE",
        )?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_>>()?;
        drop(stmt);

        let mut parts: Vec<String> = Vec::new();
        if include_index {
            let mut catalogue = vec!["# Wiki Index".to_string(), String::new()];
            let mut sorted = rows.clone();
            sorted.sort_by_key(|(title, _, _)| title.to_lowercase());
            for (title, _, summary) in &sorted {
                let tail = if summary.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", summary)
                };
                catalogue.push(format!("- [[{}]]{}", title, tail));
            }
            parts.push(catalogue.join("\n"));
        }

        let mut used = parts.first().map(|p| estimate_tokens(p)).unwrap_or(0);
        let mut included = 0;
        let mut omitted_titles: Vec<String> = Vec::new();

        for (title, content, _) in &rows {
            let block = content.trim_end().to_string();
            let cost = estimate_tokens(&block);
            // At least one page always comes back, even if it alone exceeds the
            // budget — returning nothing but an index would be useless.
            if token_budget > 0 && used + cost > token_budget && included > 0 {
                omitted_titles.push(title.clone());
                continue;
            }
            parts.push(block);
            used += cost;
            included += 1;
        }

        if !omitted_titles.is_empty() {
            parts.push(format!(
                "---\n_Omitted (token budget) — load individually with \
                 `remind_me_wiki_read`: {}_",
                omitted_titles.join(", ")
            ));
        }

        Ok(WikiLoad {
            content: parts.join("\n\n---\n\n"),
            pages_included: included,
            pages_omitted: omitted_titles.len(),
            estimated_tokens: used,
        })
    }

    /// Drive the synthesis loop over raw memories.
    ///
    /// Two phases, and the default is the safe one:
    ///
    /// 1. **Brief** (`mark_integrated: false`) returns the maintainer schema,
    ///    the current page index, and up to `limit` memories written since the
    ///    watermark. **Idempotent** — calling it repeatedly never advances
    ///    anything, which is what makes it safe to re-read.
    /// 2. **Mark integrated** advances the watermark past the surfaced batch,
    ///    after the caller has written the pages.
    ///
    /// The new watermark is the **last surfaced row's `created_at`**, not the
    /// wall clock. Using "now" would skip anything written while the caller was
    /// synthesising.
    pub fn compile(
        &self,
        conn: &Connection,
        limit: usize,
        mark_integrated: bool,
    ) -> Result<WikiCompile> {
        self.reconcile(conn)?;
        let watermark = get_meta(conn, COMPILE_WATERMARK_KEY)?.unwrap_or_default();
        let cutoff = if watermark.is_empty() {
            EPOCH.to_string()
        } else {
            watermark.clone()
        };

        let mut stmt = conn.prepare(
            "SELECT id, category, content, created_at FROM memories
              WHERE superseded_by IS NULL AND deleted_at IS NULL AND created_at > ?
              ORDER BY created_at ASC LIMIT ?",
        )?;
        let pending: Vec<(String, String, String, String)> = stmt
            .query_map(params![cutoff, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_>>()?;
        drop(stmt);

        if mark_integrated {
            let Some((_, _, _, last_created)) = pending.last() else {
                return Ok(WikiCompile::Noop {
                    reason: "no pending memories to mark".to_string(),
                    watermark,
                });
            };
            set_meta(conn, COMPILE_WATERMARK_KEY, last_created)?;
            self.append_log(&format!(
                "compiled {} source(s) — watermark -> {}",
                pending.len(),
                last_created
            ))?;
            return Ok(WikiCompile::Integrated {
                sources_marked: pending.len(),
                watermark: last_created.clone(),
            });
        }

        if pending.is_empty() {
            return Ok(WikiCompile::Noop {
                reason: "nothing pending; every raw memory is already integrated".to_string(),
                watermark,
            });
        }

        let pages = crate::wiki::list_wiki_pages(conn)?;
        let index = if pages.is_empty() {
            "_(the wiki is currently empty — you are bootstrapping it)_".to_string()
        } else {
            pages
                .iter()
                .map(|p| {
                    let tail = if p.summary.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", p.summary)
                    };
                    format!("- [[{}]]{}", p.title, tail)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let sources = pending
            .iter()
            .map(|(id, category, content, created_at)| {
                let body: String = if content.chars().count() > COMPILE_SOURCE_CHARS {
                    format!(
                        "{} …[truncated]",
                        content
                            .chars()
                            .take(COMPILE_SOURCE_CHARS)
                            .collect::<String>()
                    )
                } else {
                    content.clone()
                };
                format!("### `{}` [{}] ({})\n{}", id, category, created_at, body)
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let brief = format!(
            "# Wiki Compile Brief\n\n\
             {} pending raw memory(ies) to synthesise into the wiki (watermark: `{}`).\n\n\
             ## Maintainer schema\n\n{}\n\n\
             ## Current wiki pages\n\n{}\n\n\
             ## Pending raw sources\n\n{}\n\n\
             ## Your task\n\n\
             1. For each source above, decide which page(s) it belongs to.\n\
             2. Create or update those pages with `remind_me_wiki_write` — distil, \
             do not paste raw text; revise existing summaries; add [[cross-links]].\n\
             3. When done, call `remind_me_wiki_compile` again with \
             `mark_integrated=true` to advance the watermark.",
            pending.len(),
            if watermark.is_empty() {
                "never"
            } else {
                &watermark
            },
            self.read_schema()?.trim(),
            index,
            sources
        );

        Ok(WikiCompile::Brief {
            pending: pending.len(),
            watermark,
            brief,
        })
    }
}

/// Count raw memories awaiting wiki synthesis.
///
/// Non-superseded, non-deleted `memories` rows created after the compile
/// watermark — exactly the set [`Wiki::compile`] would surface, except
/// uncapped: `compile`'s own `pending` count is truncated by its `limit`
/// argument, which is right for a synthesis brief and wrong for a status
/// badge. Zero means the wiki is current with the memory store.
pub fn pending_compile_count(conn: &Connection) -> Result<usize> {
    let watermark = get_meta(conn, COMPILE_WATERMARK_KEY)?.unwrap_or_default();
    let cutoff = if watermark.is_empty() {
        EPOCH.to_string()
    } else {
        watermark
    };
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM memories
          WHERE superseded_by IS NULL AND deleted_at IS NULL AND created_at > ?",
        params![cutoff],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

/// Read a `wiki_meta` value.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM wiki_meta WHERE key = ?",
        params![key],
        |r| r.get(0),
    )
    .optional()
}

/// Write a `wiki_meta` value.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO wiki_meta (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn io_error(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(e.to_string())
}

fn mtime_of(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A page's title: its first H1, else the humanised slug.
pub fn extract_title(content: &str, fallback: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim().trim_end_matches('#').trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }
    fallback
        .replace('-', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A page's one-liner: its first non-heading, non-blank line.
pub fn extract_summary(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cleaned = trimmed
            .trim_start_matches(['-', '*', '>', ' '])
            .trim()
            .to_string();
        if !cleaned.is_empty() {
            return cleaned.chars().take(280).collect();
        }
    }
    String::new()
}

/// Pull `[[Link]]` and `[[Link|Alias]]` targets out of content.
///
/// Returns `(slug, title)` pairs — the slug is what resolves, the title is what
/// a reader sees, so a link survives a page being renamed in casing or
/// punctuation.
pub fn parse_wikilinks(content: &str) -> Vec<(String, String)> {
    let mut links = Vec::new();
    let mut rest = content;
    while let Some(open) = rest.find("[[") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else {
            break;
        };
        let inner = &after[..close];
        if !inner.contains('[') && !inner.contains(']') {
            // `[[Target|Alias]]` links by target and displays the alias.
            let target = inner.split('|').next().unwrap_or(inner).trim();
            if !target.is_empty() {
                links.push((slugify(target), target.to_string()));
            }
        }
        rest = &after[close + 2..];
    }
    links
}

/// Put the canonical H1 at the top of the body.
fn normalise_heading(title: &str, content: &str) -> String {
    let body = content.trim_matches('\n');
    let existing = body
        .lines()
        .find(|l| l.trim_start().starts_with("# "))
        .map(|l| l.trim().trim_start_matches("# ").trim().to_string());

    match existing {
        Some(found) if found == title => format!("{}\n", body),
        Some(_) => {
            // A different H1 is replaced, not duplicated: the file's title has
            // to match the slug it is stored under or the index lies.
            let without: String = body
                .lines()
                .skip_while(|l| !l.trim_start().starts_with("# "))
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n");
            format!("# {}\n\n{}\n", title, without.trim_matches('\n'))
        }
        None => format!("# {}\n\n{}\n", title, body),
    }
}

const DEFAULT_SCHEMA: &str = r#"# Wiki Maintainer Schema

How to write and revise pages in this wiki.

## Page shape

- One `# H1` matching the page title, first line.
- A one-sentence summary as the first body line — it becomes the index entry.
- Sections under `##` headings.

## Rules

- **Distil, do not paste.** A page is synthesised knowledge, not a transcript.
- **Revise in place.** Update the existing sentence rather than appending a
  contradicting one.
- **Cross-link** related pages with `[[Page Title]]`.
- **Flag contradictions** explicitly rather than silently choosing: note both
  claims and their sources.

## Generated pages

`index.md`, `log.md` and `schema.md` are maintained automatically. Do not edit
them by hand — `index.md` is regenerated on every write.
"#;
