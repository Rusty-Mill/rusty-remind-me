//! Dry-run plans for normalising decisions (card #1564, pack D30 to D35).
//!
//! Two read-only planners, nothing here writes to the store:
//!
//! - [`plan_normalize`]: which live memories should become
//!   `memory_type: decision`, or gain a `metadata.decision_ref`, and which
//!   cannot without a person (no rationale, a project conflict).
//! - [`plan_import`]: which entries of a repo `DECISIONS.md` are new, already
//!   in the store, or changed, plus supersedes links the file states outright.
//!
//! Anything ambiguous is reported, never guessed (D32 to D34). Plans are
//! deterministic: same input, same report (D35).

use crate::models::Memory;
use serde::{Deserialize, Serialize};

// -- refs -----------------------------------------------------------------

/// `D` followed by 1 to 4 digits, the whole token.
fn is_d_ref(token: &str) -> bool {
    let digits = token.strip_prefix('D').unwrap_or("");
    (1..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Every `D<n>` token in `text`, in order.
fn d_refs(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| is_d_ref(t))
        .map(str::to_string)
        .collect()
}

/// The ref a memory's content opens by defining: `D22: ...`, `D22 [stated]`,
/// `D22 (...)`, or the same after a project of up to three words. `None` when the
/// content only mentions a ref later on.
fn defined_ref(content: &str) -> Option<(Option<String>, String)> {
    let words: Vec<&str> = content.split_whitespace().take(5).collect();
    for (i, word) in words.iter().enumerate().take(4) {
        let token = word.trim_end_matches(':');
        if !is_d_ref(token) {
            continue;
        }
        // A definition, not a mention: `D22:` or `D22 [stated]` / `D22 (...)`.
        let marked = word.ends_with(':')
            || words
                .get(i + 1)
                .is_some_and(|next| next.starts_with(['[', '(']) || *next == "—" || *next == "-");
        if !marked {
            return None;
        }
        let project = (i > 0).then(|| words[..i].join(" "));
        return Some((project, token.to_string()));
    }
    None
}

/// The ref a memory already carries, from whichever metadata key holds it:
/// `decision_ref`, `decision_id` (bare `D136`) or `decision`
/// (`rusty_term D64`). `decision_key` holds slugs, not refs, and is skipped.
fn carried_ref(memory: &Memory) -> Option<(String, &'static str)> {
    let get = |key: &str| memory.metadata.get(key).and_then(|v| v.as_str());
    if let Some(r) = get("decision_ref").filter(|r| !r.trim().is_empty()) {
        return Some((r.trim().to_string(), "decision_ref"));
    }
    if let Some(r) = get("decision_id").filter(|r| is_d_ref(r.trim())) {
        return Some((r.trim().to_string(), "decision_id"));
    }
    get("decision")
        .and_then(|d| d_refs(d).pop())
        .map(|r| (r, "decision"))
}

/// Why the memory's refs cannot be trusted, if they cannot: the content's
/// definition and the metadata keys name different refs, or
/// `metadata.decision` names another project. A proposal would otherwise
/// bless one of them as the memory's identity.
fn ref_disagreement(memory: &Memory, defined: Option<&str>) -> Option<String> {
    let get = |key: &str| {
        memory
            .metadata
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
    };
    let decision = get("decision");
    let named: Vec<(&str, String)> = [
        ("content", defined.map(str::to_string)),
        (
            "metadata.decision_ref",
            get("decision_ref")
                .filter(|r| !r.is_empty())
                .map(str::to_string),
        ),
        (
            "metadata.decision_id",
            get("decision_id")
                .filter(|r| is_d_ref(r))
                .map(str::to_string),
        ),
        ("metadata.decision", decision.and_then(|d| d_refs(d).pop())),
    ]
    .into_iter()
    .filter_map(|(source, r)| r.map(|r| (source, r)))
    .collect();
    if named.iter().any(|(_, r)| *r != named[0].1) {
        let all: Vec<String> = named.iter().map(|(s, r)| format!("{s} {r}")).collect();
        return Some(format!("refs disagree: {}", all.join(", ")));
    }
    let qualifier = decision.map(|d| {
        d_refs(d)
            .iter()
            .fold(d.to_string(), |rest, r| rest.replace(r.as_str(), ""))
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string()
    })?;
    let field = memory.project.as_deref().unwrap_or("");
    if qualifier.is_empty() || same_project(&qualifier, field) {
        return None;
    }
    Some(format!(
        "metadata.decision names project {qualifier:?}, field is {field:?}"
    ))
}

fn metadata_rationale(memory: &Memory) -> Option<&str> {
    memory.metadata.get("rationale").and_then(|v| v.as_str())
}

fn has_rationale(memory: &Memory) -> bool {
    metadata_rationale(memory).is_some_and(|r| !r.trim().is_empty())
}

/// Project names compared loosely: case, `-`, `_` and spaces ignored, so
/// `let-em-loose`, `let_em_loose` and `Let Em Loose` agree.
fn same_project(a: &str, b: &str) -> bool {
    let key = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    };
    key(a) == key(b)
}

fn snippet(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

// -- normalise --------------------------------------------------------------

/// One memory the normalise plan names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizeRow {
    pub id: String,
    pub project: Option<String>,
    /// The ref to write to `metadata.decision_ref`, when one is known.
    pub decision_ref: Option<String>,
    /// Why the row is in its bucket.
    pub note: String,
    pub snippet: String,
}

/// What normalising would do. Only `retype` and `add_ref` are proposals; the
/// other buckets need a person.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizePlan {
    pub project: Option<String>,
    /// Set `memory_type: decision` (and `decision_ref` when known).
    pub retype: Vec<NormalizeRow>,
    /// Already a decision: copy its ref into `metadata.decision_ref`.
    pub add_ref: Vec<NormalizeRow>,
    /// Would be a decision, but has no `metadata.rationale` to keep.
    pub needs_rationale: Vec<NormalizeRow>,
    /// The content names a different project from the memory's field.
    pub conflicts: Vec<NormalizeRow>,
    /// Content mentions a ref without defining one: report only.
    pub mentions: Vec<NormalizeRow>,
}

/// Plan the normalisation of `memories`, limited to `project` when given.
/// Deleted, superseded and sensitive memories are left out.
pub fn plan_normalize(memories: &[Memory], project: Option<&str>) -> NormalizePlan {
    let mut plan = NormalizePlan {
        project: project.map(str::to_string),
        ..Default::default()
    };
    let mut live: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.deleted_at.is_none() && m.superseded_by.is_none() && !m.sensitive)
        .filter(|m| {
            project.is_none_or(|p| m.project.as_deref().is_some_and(|q| same_project(p, q)))
        })
        .collect();
    live.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    for memory in live {
        classify(memory, &mut plan);
    }
    plan
}

fn classify(memory: &Memory, plan: &mut NormalizePlan) {
    let is_decision = memory.memory_type.as_deref() == Some("decision");
    let defined = defined_ref(&memory.content);
    let carried = carried_ref(memory);
    let row = |decision_ref: Option<String>, note: String| NormalizeRow {
        id: memory.id.clone(),
        project: memory.project.clone(),
        decision_ref,
        note,
        snippet: snippet(&memory.content),
    };

    if let Some((Some(named), r)) = &defined {
        let field = memory.project.as_deref().unwrap_or("");
        if field.is_empty() || !same_project(named, field) {
            let note = format!("content names project {named:?}, field is {field:?}");
            plan.conflicts.push(row(Some(r.clone()), note));
            return;
        }
    }
    if let Some(note) = ref_disagreement(memory, defined.as_ref().map(|(_, r)| r.as_str())) {
        plan.conflicts.push(row(None, note));
        return;
    }
    let wanted_ref = carried
        .as_ref()
        .map(|(r, _)| r.clone())
        .or_else(|| defined.as_ref().map(|(_, r)| r.clone()));

    if is_decision {
        match carried {
            Some((_, "decision_ref")) => {}
            Some((r, key)) => plan
                .add_ref
                .push(row(Some(r), format!("from metadata.{key}"))),
            None if defined.is_some() => plan
                .add_ref
                .push(row(wanted_ref, "from the content's opening".into())),
            None => {}
        }
        return;
    }

    let candidate = memory.category == "decision" || defined.is_some();
    if candidate && !has_rationale(memory) {
        let why = if memory.category == "decision" {
            "category decision"
        } else {
            "content defines a ref"
        };
        plan.needs_rationale.push(row(wanted_ref, why.into()));
        return;
    }
    if candidate {
        let note = format!(
            "memory_type {} -> decision",
            memory.memory_type.as_deref().unwrap_or("none")
        );
        plan.retype.push(row(wanted_ref, note));
        return;
    }
    let mentioned = d_refs(&memory.content);
    if !mentioned.is_empty() {
        plan.mentions
            .push(row(None, format!("mentions {}", mentioned.join(", "))));
    }
}

// -- DECISIONS.md parsing -----------------------------------------------------

/// One decision read from a log file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// As written: `D22`, or a slug key such as `entry-rule`.
    pub decision_ref: String,
    pub title: String,
    pub rationale: Option<String>,
    pub date: Option<String>,
    pub tag: Option<String>,
    /// 1-based line of the row or heading.
    pub line: usize,
    /// Struck through (`~~...~~`): kept for the report, never imported live.
    pub struck: bool,
    /// A section's `**Decision.**` paragraph; a table row has only its title.
    pub decision: Option<String>,
}

impl LogEntry {
    /// What the entry decides: its title and, for a section, the decision
    /// paragraph. Rationale, rejected options and notes are left out, so a
    /// link quoted there is never read as the entry's own.
    fn statement(&self) -> String {
        match &self.decision {
            Some(body) => format!("{} {body}", self.title),
            None => self.title.clone(),
        }
    }
}

/// A line or row the parser could not read as a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unparsed {
    pub line: usize,
    pub reason: String,
    pub snippet: String,
}

/// A parsed log: entries in file order, and what was skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedLog {
    pub entries: Vec<LogEntry>,
    pub unparsed: Vec<Unparsed>,
}

/// Parse a `DECISIONS.md`: markdown tables mapped by header name, and
/// `## D<n>. Title` sections. A ref in both a table and a section is read
/// from the section, which carries the fuller text.
pub fn parse_log(text: &str) -> ParsedLog {
    let lines: Vec<&str> = text.lines().collect();
    let mut log = ParsedLog::default();
    let sections = parse_sections(&lines);
    let mut i = 0;
    while i < lines.len() {
        if is_table_start(&lines, i) {
            i = parse_table(&lines, i, &mut log);
        } else {
            i += 1;
        }
    }
    log.entries
        .retain(|e| !sections.iter().any(|s| s.decision_ref == e.decision_ref));
    log.entries.extend(sections);
    log.entries.sort_by_key(|e| e.line);
    log
}

fn cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out.into_iter().map(|c| c.trim().to_string()).collect()
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| "|-: ".contains(c))
}

fn is_table_start(lines: &[&str], i: usize) -> bool {
    lines[i].trim().starts_with('|') && lines.get(i + 1).is_some_and(|l| is_separator(l))
}

/// Column positions by header name.
#[derive(Default)]
struct Columns {
    reference: Option<usize>,
    decision: Option<usize>,
    why: Option<usize>,
    date: Option<usize>,
    tag: Option<usize>,
}

fn columns(header: &[String]) -> Columns {
    let mut cols = Columns::default();
    for (i, name) in header.iter().enumerate() {
        let name = name.trim_matches('*').to_ascii_lowercase();
        let slot = match name.as_str() {
            "#" | "d" | "id" | "key" | "ref" => &mut cols.reference,
            "decision" => &mut cols.decision,
            "why" | "rationale" | "reason" => &mut cols.why,
            "date" => &mut cols.date,
            "tag" | "tier" => &mut cols.tag,
            _ => continue,
        };
        slot.get_or_insert(i);
    }
    cols
}

/// Parse the table starting at `start`; returns the line after it.
fn parse_table(lines: &[&str], start: usize, log: &mut ParsedLog) -> usize {
    let header = cells(lines[start]);
    let mut cols = columns(&header);
    let mut i = start + 2;
    // A table whose first column holds refs but has an unnamed header.
    if cols.reference.is_none() {
        let first = lines.get(i).map(|l| cells(l)).unwrap_or_default();
        if first.first().is_some_and(|c| is_d_ref(clean(c).0.as_str())) {
            cols.reference = Some(0);
        }
    }
    loop {
        while i < lines.len() && lines[i].trim().starts_with('|') {
            let row = cells(lines[i]);
            read_row(&row, &cols, i + 1, lines[i], log);
            i += 1;
        }
        // Rows after a stray blank line, with no header of their own, still
        // belong to this table.
        let next = (i..lines.len()).find(|&j| !lines[j].trim().is_empty());
        match next {
            Some(j) if lines[j].trim().starts_with('|') && !is_table_start(lines, j) => i = j,
            _ => return i,
        }
    }
}

/// Strip bold and strike-through marks; `true` when the text was struck.
fn clean(cell: &str) -> (String, bool) {
    let struck = cell.starts_with("~~") && cell.ends_with("~~") && cell.len() > 4;
    let text = cell.replace("~~", "").replace("**", "");
    (text.trim().to_string(), struck)
}

fn read_row(row: &[String], cols: &Columns, line: usize, raw: &str, log: &mut ParsedLog) {
    let at = |c: Option<usize>| c.and_then(|i| row.get(i)).map(|s| clean(s));
    let unparsed = |reason: &str| Unparsed {
        line,
        reason: reason.to_string(),
        snippet: snippet(raw),
    };
    let Some((reference, ref_struck)) = at(cols.reference).filter(|(r, _)| !r.is_empty()) else {
        log.unparsed.push(unparsed("no ref column"));
        return;
    };
    let decision = at(cols.decision).or_else(|| {
        let i = cols.reference.map_or(1, |r| r + 1);
        row.get(i).map(|s| clean(s))
    });
    let Some((title, title_struck)) = decision.filter(|(t, _)| !t.is_empty()) else {
        log.unparsed.push(unparsed("no decision text"));
        return;
    };
    let some = |c: Option<usize>| at(c).map(|(t, _)| t).filter(|t| !t.is_empty());
    log.entries.push(LogEntry {
        decision_ref: reference,
        title,
        rationale: some(cols.why),
        date: some(cols.date),
        tag: some(cols.tag),
        line,
        struck: ref_struck || title_struck,
        decision: None,
    });
}

/// `## D<n>. Title` (or `###`) sections, with the `**Decision.**` and
/// `**Why.**` paragraphs as decision and rationale.
fn parse_sections(lines: &[&str]) -> Vec<LogEntry> {
    let mut out: Vec<LogEntry> = Vec::new();
    let mut open_section = false;
    let mut field = Field::Other;
    for (i, line) in lines.iter().enumerate() {
        if let Some(entry) = section_heading(line, i + 1) {
            out.push(entry);
            open_section = true;
            field = Field::Other;
            continue;
        }
        if line.starts_with('#') {
            open_section = false;
        }
        let Some(open) = out.last_mut().filter(|_| open_section) else {
            continue;
        };
        field = section_line(open, line.trim(), field);
    }
    out
}

/// The section paragraph a continuation line belongs to.
#[derive(Clone, Copy)]
enum Field {
    Decision,
    Why,
    Other,
}

/// Fold one body line into `entry`; returns the paragraph the next line
/// continues. A blank line or another `**Label.**` ends a paragraph.
fn section_line(entry: &mut LogEntry, line: &str, field: Field) -> Field {
    if line.is_empty() {
        return Field::Other;
    }
    if let Some(rest) = line.strip_prefix("**Decision.**") {
        append(&mut entry.decision, rest);
        return Field::Decision;
    }
    if let Some(rest) = line.strip_prefix("**Why.**") {
        append(&mut entry.rationale, rest);
        return Field::Why;
    }
    if line.starts_with("**") {
        return Field::Other;
    }
    match field {
        Field::Decision => append(&mut entry.decision, line),
        Field::Why => append(&mut entry.rationale, line),
        Field::Other => {}
    }
    field
}

fn append(slot: &mut Option<String>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    match slot {
        Some(held) => {
            held.push(' ');
            held.push_str(text);
        }
        None => *slot = Some(text.to_string()),
    }
}

fn section_heading(line: &str, number: usize) -> Option<LogEntry> {
    let rest = line
        .strip_prefix("## ")
        .or_else(|| line.strip_prefix("### "))?;
    let (reference, title) = rest.split_once(['.', ' '])?;
    if !is_d_ref(reference) {
        return None;
    }
    let (title, struck) = clean(title.trim_start_matches('.').trim());
    Some(LogEntry {
        decision_ref: reference.to_string(),
        title: title.clone(),
        rationale: None,
        date: None,
        tag: None,
        line: number,
        struck,
        decision: None,
    })
}

// -- import -------------------------------------------------------------------

/// An entry and what importing it would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRow {
    pub decision_ref: String,
    pub line: usize,
    /// In full: the report is what a person approves, so nothing is cut.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The matching memory, when the store already holds this ref.
    pub memory_id: Option<String>,
    /// A changed row's stored content and rationale, to set beside the file's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_rationale: Option<String>,
    pub note: String,
}

impl ImportRow {
    fn of(entry: &LogEntry, memory_id: Option<String>, note: &str) -> Self {
        Self {
            decision_ref: entry.decision_ref.clone(),
            line: entry.line,
            title: entry.title.clone(),
            decision: entry.decision.clone(),
            rationale: entry.rationale.clone(),
            date: entry.date.clone(),
            tag: entry.tag.clone(),
            memory_id,
            stored: None,
            stored_rationale: None,
            note: note.to_string(),
        }
    }
}

/// A supersedes link the file states outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersedesRow {
    pub decision_ref: String,
    pub supersedes: String,
    pub line: usize,
}

/// What importing one log would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPlan {
    pub project: String,
    pub source: String,
    pub new: Vec<ImportRow>,
    pub unchanged: Vec<ImportRow>,
    /// The store holds the ref with different text.
    pub changed: Vec<ImportRow>,
    /// New, but with no rationale in the file.
    pub needs_rationale: Vec<ImportRow>,
    /// The same ref twice in the file, or struck-through rows.
    pub conflicts: Vec<ImportRow>,
    pub supersedes: Vec<SupersedesRow>,
    /// `confirms D<n>`: recorded as `metadata.confirms`, the target stays open.
    pub confirms: Vec<SupersedesRow>,
    /// Supersede wording the plan will not act on (cross-project, prose).
    pub ambiguous: Vec<ImportRow>,
    pub unparsed: Vec<Unparsed>,
}

/// Plan importing `log` (read from `source`) into `project`, against the
/// live memories already in the store.
pub fn plan_import(
    log: &ParsedLog,
    project: &str,
    source: &str,
    memories: &[Memory],
) -> ImportPlan {
    let mut plan = ImportPlan {
        project: project.to_string(),
        source: source.to_string(),
        unparsed: log.unparsed.clone(),
        ..Default::default()
    };
    let existing: Vec<(&Memory, String)> = memories
        .iter()
        .filter(|m| m.deleted_at.is_none() && m.superseded_by.is_none())
        .filter(|m| m.memory_type.as_deref() == Some("decision"))
        .filter(|m| {
            m.project
                .as_deref()
                .is_some_and(|p| same_project(p, project))
        })
        .filter_map(|m| carried_ref(m).map(|(r, _)| (m, r)))
        .collect();
    for entry in &log.entries {
        if place(entry, log, &existing, &mut plan) {
            links(entry, &mut plan);
        } else if has_link_wording(&entry.statement()) {
            plan.ambiguous
                .push(ambiguous_row(entry, "link wording on a conflicting entry"));
        }
    }
    plan
}

/// Sort `entry` into its bucket. `false` when it is a conflict, whose links
/// are then only reported, never proposed.
fn place(
    entry: &LogEntry,
    log: &ParsedLog,
    existing: &[(&Memory, String)],
    plan: &mut ImportPlan,
) -> bool {
    let row = |memory_id: Option<String>, note: &str| ImportRow::of(entry, memory_id, note);
    let twins = log
        .entries
        .iter()
        .filter(|e| e.decision_ref == entry.decision_ref)
        .count();
    if twins > 1 {
        plan.conflicts.push(row(None, "ref appears more than once"));
        return false;
    }
    if entry.struck {
        plan.conflicts.push(row(None, "struck through"));
        return false;
    }
    let matches: Vec<&&Memory> = existing
        .iter()
        .filter(|(_, r)| *r == entry.decision_ref)
        .map(|(m, _)| m)
        .collect();
    match matches.as_slice() {
        [] if entry.rationale.is_none() => plan
            .needs_rationale
            .push(row(None, "no rationale column or Why")),
        [] => plan.new.push(row(None, "not in the store")),
        [one] => match difference(one, entry) {
            None => plan
                .unchanged
                .push(row(Some(one.id.clone()), "same ref, text and rationale")),
            Some(what) => plan.changed.push(ImportRow {
                stored: Some(one.content.clone()),
                stored_rationale: metadata_rationale(one).map(str::to_string),
                ..row(Some(one.id.clone()), what)
            }),
        },
        _ => {
            plan.conflicts
                .push(row(None, "several memories carry this ref"));
            return false;
        }
    }
    true
}

/// What differs between a stored decision and the file's entry; `None` only
/// when the words match exactly. Anything short of that counts as changed: a
/// false "unchanged" would hide an edit from the person approving the plan.
fn difference(memory: &Memory, entry: &LogEntry) -> Option<&'static str> {
    let stored = without_ref(words(&memory.content), &entry.decision_ref);
    if stored != words(&entry.statement()) {
        return Some("decision text differs");
    }
    let stored_why = metadata_rationale(memory)
        .map(words)
        .filter(|w| !w.is_empty());
    let file_why = entry
        .rationale
        .as_deref()
        .map(words)
        .filter(|w| !w.is_empty());
    match (stored_why, file_why) {
        (a, b) if a == b => None,
        (None, Some(_)) => Some("same text; the store has no rationale"),
        _ => Some("same text, different rationale"),
    }
}

/// Markup and punctuation that never change what a decision says.
const IGNORED: &str = "*_`~.,;:'\"()[]|‘’“”";

/// Lowercase words for comparing decisions. Markup, punctuation and spacing
/// are ignored; every other symbol is a token of its own, so `-1` differs
/// from `1` and `< 100` from `> 100`. A symbol not known to be harmless
/// counts, so doubt lands in "changed", never "unchanged".
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
            continue;
        }
        if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        if !c.is_whitespace() && !IGNORED.contains(c) {
            out.push(c.to_string());
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// `words` without a leading definition of `reference`: an optional project
/// (up to three words, hyphens included), the ref, a `stated` or `default`
/// tag, and a `-` or `—` marker.
fn without_ref(mut words: Vec<String>, reference: &str) -> Vec<String> {
    let target = reference.to_lowercase();
    let Some(mut end) = words.iter().take(8).position(|w| *w == target) else {
        return words;
    };
    for skippable in [["stated", "default"], ["-", "—"]] {
        if words
            .get(end + 1)
            .is_some_and(|w| skippable.contains(&w.as_str()))
        {
            end += 1;
        }
    }
    words.drain(..=end);
    words
}

/// Marks that open a quotation.
const QUOTES: [char; 3] = ['"', '“', '`'];

/// Words that turn the link after them around.
const NEGATIONS: [&str; 6] = ["not", "never", "no", "nor", "doesn't", "don't"];

/// `supersedes D<n>` / `confirms D<n>` stated outright in the entry's own
/// decision. Negated, quoted or partial links, and other supersede wording,
/// are ambiguous: reported, never proposed (D34).
fn links(entry: &LogEntry, plan: &mut ImportPlan) {
    let text = entry.statement();
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut explicit = false;
    let mut doubt = None;
    for (i, pair) in words.windows(2).enumerate() {
        let keyword = pair[0]
            .trim_start_matches(QUOTES)
            .trim_start_matches('(')
            .to_ascii_lowercase();
        let target = pair[1].trim_end_matches([',', '.', ';', ':', ')', '"', '”', '`']);
        if !matches!(keyword.as_str(), "supersedes" | "confirms") || !is_d_ref(target) {
            continue;
        }
        if let Some(why) = hedged(&words, i) {
            doubt = Some(why);
            continue;
        }
        let link = SupersedesRow {
            decision_ref: entry.decision_ref.clone(),
            supersedes: target.to_string(),
            line: entry.line,
        };
        if keyword == "supersedes" {
            explicit = true;
            plan.supersedes.push(link);
        } else {
            plan.confirms.push(link);
        }
    }
    let note = match doubt {
        Some(why) => why,
        None if !explicit && text.to_ascii_lowercase().contains("supersede") => {
            "supersede wording without a same-project ref"
        }
        None => return,
    };
    plan.ambiguous.push(ambiguous_row(entry, note));
}

/// Why the link whose keyword is `words[i]` (its ref at `words[i + 1]`) is
/// not a plain statement: negated, quoted, or partial (`D2 in part`,
/// `D2 item 4`, `... only`).
fn hedged(words: &[&str], i: usize) -> Option<&'static str> {
    let previous = i.checked_sub(1).map(|p| words[p].to_ascii_lowercase());
    if previous.is_some_and(|w| NEGATIONS.contains(&w.as_str())) {
        return Some("negated link");
    }
    let keyword = words[i];
    let lead = &keyword[..keyword.len() - keyword.trim_start_matches(QUOTES).len()];
    let before = format!("{} {lead}", words[..i].join(" "));
    let open_curly = before.matches('“').count() > before.matches('”').count();
    let odd = |mark: char| before.matches(mark).count() % 2 == 1;
    if open_curly || odd('"') || odd('`') {
        return Some("quoted link");
    }
    let target = words[i + 1];
    let ends_phrase =
        i + 2 == words.len() || target.ends_with([',', '.', ';', ':', ')', '"', '”', '`']);
    if !ends_phrase {
        return Some("partial link (words follow the ref)");
    }
    let clause = words[i..]
        .iter()
        .position(|w| w.ends_with(['.', ';']))
        .map_or(&words[i..], |end| &words[i..=i + end]);
    if clause.iter().any(|w| {
        w.trim_matches(|c: char| !c.is_alphanumeric())
            .eq_ignore_ascii_case("only")
    }) {
        return Some("partial link (the clause says only)");
    }
    None
}

fn has_link_wording(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("supersede") || lower.contains("confirms")
}

fn ambiguous_row(entry: &LogEntry, note: &str) -> ImportRow {
    ImportRow::of(entry, None, note)
}

// -- report -------------------------------------------------------------------

fn section<T>(out: &mut String, title: &str, rows: &[T], line: impl Fn(&T) -> String) {
    out.push_str(&format!("\n## {title} ({})\n\n", rows.len()));
    if rows.is_empty() {
        out.push_str("None.\n");
        return;
    }
    for r in rows {
        out.push_str(&format!("- {}\n", line(r)));
    }
}

fn norm_line(r: &NormalizeRow) -> String {
    format!(
        "`{}` [{}] {} — {}: {}",
        r.id,
        r.project.as_deref().unwrap_or("no project"),
        r.decision_ref.as_deref().unwrap_or("no ref"),
        r.note,
        r.snippet
    )
}

/// The normalise plan as markdown: counts, then every row per bucket.
pub fn render_normalize(plan: &NormalizePlan) -> String {
    let scope = plan.project.as_deref().unwrap_or("all projects");
    let mut out = format!(
        "# Decisions normalise dry run: {scope}\n\nNo memory was added, changed or deleted. Proposed: {} retype, {} add ref. Needs a person: {} need rationale, {} conflicts. Report only: {} mentions.\n",
        plan.retype.len(),
        plan.add_ref.len(),
        plan.needs_rationale.len(),
        plan.conflicts.len(),
        plan.mentions.len()
    );
    section(&mut out, "Retype to decision", &plan.retype, norm_line);
    section(
        &mut out,
        "Add metadata.decision_ref",
        &plan.add_ref,
        norm_line,
    );
    section(
        &mut out,
        "Needs rationale",
        &plan.needs_rationale,
        norm_line,
    );
    section(&mut out, "Project conflicts", &plan.conflicts, norm_line);
    section(&mut out, "Mentions only", &plan.mentions, norm_line);
    out
}

fn import_line(r: &ImportRow) -> String {
    let memory = r
        .memory_id
        .as_deref()
        .map(|m| format!(" `{m}`"))
        .unwrap_or_default();
    let mut line = format!(
        "{} (line {}){memory} — {}: {}",
        r.decision_ref, r.line, r.note, r.title
    );
    let details = [
        ("Decision", &r.decision),
        ("Why", &r.rationale),
        ("Date", &r.date),
        ("Tag", &r.tag),
        ("Stored", &r.stored),
        ("Stored why", &r.stored_rationale),
    ];
    for (label, value) in details {
        if let Some(value) = value {
            line.push_str(&format!("\n  - {label}: {}", one_line(value)));
        }
    }
    line
}

/// `text` on one line, so it stays inside its list item.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn link_line(r: &SupersedesRow) -> String {
    format!("{} -> {} (line {})", r.decision_ref, r.supersedes, r.line)
}

/// The import plan as markdown: counts, then every row per bucket.
pub fn render_import(plan: &ImportPlan) -> String {
    let mut out = format!(
        "# Decisions import dry run: {} from {}\n\nNo memory was added, changed or deleted. New {}, unchanged {}, changed {}, needs rationale {}, conflicts {}, supersedes {}, confirms {}, ambiguous {}, unparsed {}.\n",
        plan.project,
        plan.source,
        plan.new.len(),
        plan.unchanged.len(),
        plan.changed.len(),
        plan.needs_rationale.len(),
        plan.conflicts.len(),
        plan.supersedes.len(),
        plan.confirms.len(),
        plan.ambiguous.len(),
        plan.unparsed.len()
    );
    section(&mut out, "New", &plan.new, import_line);
    section(&mut out, "Changed", &plan.changed, import_line);
    section(&mut out, "Unchanged", &plan.unchanged, import_line);
    section(
        &mut out,
        "Needs rationale",
        &plan.needs_rationale,
        import_line,
    );
    section(&mut out, "Conflicts", &plan.conflicts, import_line);
    section(&mut out, "Supersedes", &plan.supersedes, link_line);
    section(&mut out, "Confirms", &plan.confirms, link_line);
    section(&mut out, "Ambiguous", &plan.ambiguous, import_line);
    section(&mut out, "Unparsed", &plan.unparsed, |u| {
        format!("line {} — {}: {}", u.line, u.reason, u.snippet)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_whole_d_tokens() {
        assert!(is_d_ref("D7") && is_d_ref("D1234"));
        assert!(!is_d_ref("D") && !is_d_ref("D12345") && !is_d_ref("DX1") && !is_d_ref("d7"));
        assert_eq!(d_refs("see D7, (D12) and ID9"), vec!["D7", "D12"]);
    }

    #[test]
    fn defined_ref_reads_the_opening_only() {
        assert_eq!(defined_ref("D22: keep it"), Some((None, "D22".into())));
        assert_eq!(
            defined_ref("rusty-remind-me D1 [stated]: one memory"),
            Some((Some("rusty-remind-me".into()), "D1".into()))
        );
        assert_eq!(
            defined_ref("Let Em Loose D50 [default] ..."),
            Some((Some("Let Em Loose".into()), "D50".into()))
        );
        assert_eq!(defined_ref("We followed D50 here"), None);
        assert_eq!(defined_ref("Per D50, nothing"), None);
    }

    #[test]
    fn projects_compare_loosely() {
        assert!(same_project("let-em-loose", "Let Em Loose"));
        assert!(same_project("let_em_loose", "let-em-loose"));
        assert!(!same_project("mill", "millwright"));
    }

    #[test]
    fn cells_keep_escaped_pipes() {
        assert_eq!(cells("| a \\| b | c |"), vec!["a | b", "c"]);
    }
}
