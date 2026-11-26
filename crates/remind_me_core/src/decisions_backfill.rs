//! Plans for normalising decisions (card #1564, pack D30 to D35).
//!
//! Two read-only planners:
//!
//! - [`plan_normalize`]: which live memories should become
//!   `memory_type: decision`, or gain a `metadata.decision_ref`, and which
//!   cannot without a person (no rationale, a project conflict).
//! - [`plan_import`]: which entries of a repo `DECISIONS.md` are new, already
//!   in the store, or changed, plus supersedes links the file states outright.
//!
//! One writer, [`apply_normalize_with`], carries out the plan's proposals:
//! `respell` (an explicit project fold, D71), then `retype` (D72), then
//! `add_ref`. Every other bucket still needs a person.
//!
//! Anything ambiguous is reported, never guessed (D32 to D34). Plans are
//! deterministic: same input, same report (D35).

use crate::db::history::{Revisions, Tracked};
use crate::db::memories::{Memories, MemoryEdit};
use crate::db::queries::{self, PreparedAdd};
use crate::db::{Store, StoreError};
use crate::kinds::StructuredFields;
use crate::models::Memory;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// -- refs -----------------------------------------------------------------

/// `D` followed by 1 to 4 digits, the whole token.
fn is_d_ref(token: &str) -> bool {
    let digits = token.strip_prefix('D').unwrap_or("");
    (1..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// A ref a link may name: `D<n>`, or a renamed repeat `D<n>b` to `D<n>z`
/// ([`RefOptions::rename_duplicates`]).
fn is_link_target(token: &str) -> bool {
    match token.strip_suffix(|c: char| ('b'..='z').contains(&c)) {
        Some(base) if is_d_ref(base) => true,
        _ => is_d_ref(token),
    }
}

/// Every `D<n>` token in `text`, in order.
fn d_refs(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| is_d_ref(t))
        .map(str::to_string)
        .collect()
}

/// Whether the word before a ref is a link keyword (`Supersedes D1: ...`,
/// `confirms D7 (...)`), read as [`links`] reads it: the ref is a link's
/// target, never the content defining it.
fn follows_link_keyword(before: &[&str]) -> bool {
    before.last().is_some_and(|word| {
        let keyword = word
            .trim_start_matches(QUOTES)
            .trim_start_matches('(')
            .to_ascii_lowercase();
        matches!(keyword.as_str(), "supersedes" | "confirms")
    })
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
        if !marked || follows_link_keyword(&words[..i]) {
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
    /// For a `respell` row, the fold planned for it: the write skips the
    /// row if its project no longer matches this fold's source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold: Option<Fold>,
    /// Why the row is in its bucket.
    pub note: String,
    pub snippet: String,
}

/// What normalising would do. Only `retype` and `add_ref` are proposals; the
/// other buckets need a person.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizePlan {
    pub project: Option<String>,
    /// The folds this plan was made with (`--fold FROM=TO`).
    #[serde(default)]
    pub folds: Vec<Fold>,
    /// Rename `project` from a fold's `from` to its `to` (D71). Any type.
    #[serde(default)]
    pub respell: Vec<NormalizeRow>,
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

/// A project fold: memories whose project is exactly `from` are renamed to
/// `to` (pack D71, D83). Only two spellings of one name may be folded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fold {
    pub from: String,
    pub to: String,
}

impl Fold {
    /// Parse `FROM=TO`.
    ///
    /// # Errors
    ///
    /// No `=`, a blank side, the same spelling twice, or two names that are
    /// not spellings of one project (`same_project`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (from, to) = raw
            .split_once('=')
            .ok_or_else(|| format!("fold {raw:?} is not FROM=TO"))?;
        let (from, to) = (from.trim(), to.trim());
        if from.is_empty() || to.is_empty() {
            return Err(format!("fold {raw:?} has a blank side"));
        }
        if from == to {
            return Err(format!("fold {raw:?} changes nothing"));
        }
        if !same_project(from, to) {
            return Err(format!(
                "fold {raw:?}: {from:?} and {to:?} are different projects, not two spellings"
            ));
        }
        Ok(Self {
            from: from.to_string(),
            to: to.to_string(),
        })
    }
}

/// A validated set of folds: each spelling is folded at most once, and no
/// fold's target is another fold's source. So an apply is idempotent: what
/// one run renames, no fold of the same set renames again (an inverse pair
/// `a=b`, `b=a` would flip rows on every run).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folds(Vec<Fold>);

impl Folds {
    /// Validate `folds` as one set.
    ///
    /// # Errors
    ///
    /// A spelling folded twice, or a fold whose target another fold renames
    /// (a chain, an inverse pair or a cycle).
    pub fn new(folds: Vec<Fold>) -> Result<Self, String> {
        let mut sources: HashSet<&str> = HashSet::new();
        for fold in &folds {
            if !sources.insert(&fold.from) {
                return Err(format!("{:?} is folded more than once", fold.from));
            }
        }
        if let Some(fold) = folds.iter().find(|f| sources.contains(f.to.as_str())) {
            return Err(format!(
                "fold {}={} targets a spelling another fold renames; folds may not chain or cycle",
                fold.from, fold.to
            ));
        }
        Ok(Self(folds))
    }

    /// The folds, in the order given.
    pub fn as_slice(&self) -> &[Fold] {
        &self.0
    }

    /// The fold that renames `memory`'s project, if any.
    fn for_memory(&self, memory: &Memory) -> Option<&Fold> {
        let project = memory.project.as_deref()?;
        self.0.iter().find(|f| f.from == project)
    }
}

/// Plan the normalisation of `memories`, limited to `project` when given.
/// Deleted, superseded and sensitive memories are left out. No folds.
pub fn plan_normalize(memories: &[Memory], project: Option<&str>) -> NormalizePlan {
    plan_normalize_with(memories, project, &Folds::default())
}

/// [`plan_normalize`] with project `folds`: every memory, of any type, whose
/// project is exactly a fold's `from` is proposed for `respell`.
pub fn plan_normalize_with(
    memories: &[Memory],
    project: Option<&str>,
    folds: &Folds,
) -> NormalizePlan {
    let mut plan = NormalizePlan {
        project: project.map(str::to_string),
        folds: folds.as_slice().to_vec(),
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
        respell(memory, folds, &mut plan);
        classify(memory, &mut plan);
    }
    plan
}

fn respell(memory: &Memory, folds: &Folds, plan: &mut NormalizePlan) {
    let Some(fold) = folds.for_memory(memory) else {
        return;
    };
    plan.respell.push(NormalizeRow {
        id: memory.id.clone(),
        project: memory.project.clone(),
        decision_ref: None,
        fold: Some(fold.clone()),
        note: format!("project {} -> {}", fold.from, fold.to),
        snippet: snippet(&memory.content),
    });
}

fn classify(memory: &Memory, plan: &mut NormalizePlan) {
    let is_decision = memory.memory_type.as_deref() == Some("decision");
    let defined = defined_ref(&memory.content);
    let carried = carried_ref(memory);
    let row = |decision_ref: Option<String>, note: String| NormalizeRow {
        id: memory.id.clone(),
        project: memory.project.clone(),
        decision_ref,
        fold: None,
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

/// What [`apply_normalize_with`] wrote, and what it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizeApplied {
    pub project: Option<String>,
    /// `respell` rows now carrying their fold's project.
    #[serde(default)]
    pub respelled: Vec<NormalizeRow>,
    /// `retype` rows now typed `decision`.
    #[serde(default)]
    pub retyped: Vec<NormalizeRow>,
    /// `add_ref` rows now carrying `metadata.decision_ref`.
    pub updated: Vec<NormalizeRow>,
    /// Planned rows that no longer qualified when their turn came (edited,
    /// deleted, or given another ref meanwhile); `note` says why. Untouched.
    pub skipped: Vec<NormalizeRow>,
    /// Rows whose write failed, with the error in `note`. Untouched.
    pub failed: Vec<NormalizeRow>,
    /// The plan this run applied, planned against the store as it stood.
    pub plan: NormalizePlan,
}

/// [`apply_normalize_with`] without folds.
///
/// # Errors
///
/// A store error reading the memories. Per-row outcomes are in the result.
pub fn apply_normalize(
    store: &Store<'_>,
    project: Option<&str>,
) -> crate::db::Result<NormalizeApplied> {
    apply_normalize_with(store, project, &Folds::default())
}

/// Plan afresh from `store`, then write the plan's proposals in order:
/// `respell`, then `retype`, then `add_ref` (pack D83). Each row is re-read
/// and re-planned in its own transaction, so a row the earlier steps
/// changed is judged as it now stands. A second run finds nothing to do.
///
/// Retyped rows get no review scheduled (D74): the write goes through the
/// storage edit, not `update_memory`.
///
/// # Errors
///
/// A store error reading the memories. Per-row outcomes are in the result.
pub fn apply_normalize_with(
    store: &Store<'_>,
    project: Option<&str>,
    folds: &Folds,
) -> crate::db::Result<NormalizeApplied> {
    let plan = plan_normalize_with(&Memories::new(store).all_live()?, project, folds);
    let scope = Scope { project, folds };
    let mut applied = NormalizeApplied {
        project: project.map(str::to_string),
        ..Default::default()
    };
    for (step, rows) in [
        (Step::Respell, &plan.respell),
        (Step::Retype, &plan.retype),
        (Step::AddRef, &plan.add_ref),
    ] {
        apply_rows(store, &scope, step, rows, &record_revision, &mut applied);
    }
    applied.plan = plan;
    Ok(applied)
}

/// Copy each planned row's ref into `metadata.decision_ref`, merged into the
/// row's metadata as it stands at write time.
///
/// Each row is one transaction: re-read the memory, plan it again, and write
/// only if it still wants the same change, merging into the metadata just
/// read. The engine's page gate keeps every other writer, background sync
/// included, out between that read and the write, so a newer edit is never
/// discarded and a row that stopped qualifying is skipped. A project or
/// topic claim row is skipped too, as every generic edit refuses it.
///
/// The row edit is in the page and rolls back with it; the revision table is
/// not. So the revision, holding what the edit replaced, is written last,
/// after the row: if it fails, the page is abandoned and neither lands, and
/// nothing that can fail runs after it but the page's own finish. A row that
/// fails or is skipped does not stop the rest.
pub fn apply_planned(
    store: &Store<'_>,
    project: Option<&str>,
    rows: &[NormalizeRow],
) -> NormalizeApplied {
    let mut applied = NormalizeApplied {
        project: project.map(str::to_string),
        ..Default::default()
    };
    let folds = Folds::default();
    let scope = Scope {
        project,
        folds: &folds,
    };
    apply_rows(
        store,
        &scope,
        Step::AddRef,
        rows,
        &record_revision,
        &mut applied,
    );
    applied
}

/// Writes the revision of a row's replaced values.
type Recorder = dyn Fn(&Store<'_>, &str, &Tracked) -> crate::db::Result<()>;

fn record_revision(store: &Store<'_>, id: &str, before: &Tracked) -> crate::db::Result<()> {
    Revisions::new(store).insert(id, before, &chrono::Utc::now().to_rfc3339(), None)
}

/// What a plan was made with, so a row can be planned again at write time.
struct Scope<'a> {
    project: Option<&'a str>,
    folds: &'a Folds,
}

/// One kind of proposal [`apply_normalize_with`] carries out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Respell,
    Retype,
    AddRef,
}

impl Step {
    fn bucket(self, plan: &NormalizePlan) -> &[NormalizeRow] {
        match self {
            Self::Respell => &plan.respell,
            Self::Retype => &plan.retype,
            Self::AddRef => &plan.add_ref,
        }
    }

    fn done(self, applied: &mut NormalizeApplied) -> &mut Vec<NormalizeRow> {
        match self {
            Self::Respell => &mut applied.respelled,
            Self::Retype => &mut applied.retyped,
            Self::AddRef => &mut applied.updated,
        }
    }
}

fn apply_rows(
    store: &Store<'_>,
    scope: &Scope<'_>,
    step: Step,
    rows: &[NormalizeRow],
    record: &Recorder,
    applied: &mut NormalizeApplied,
) {
    for row in rows {
        let noted = |note: String| NormalizeRow {
            note,
            ..row.clone()
        };
        match store.transaction(|page| write_normalized(page, scope, step, row, record)) {
            Ok(NormWrite::Written) => step.done(applied).push(row.clone()),
            Ok(NormWrite::Skipped(why)) => applied.skipped.push(noted(why)),
            Err(e) => applied.failed.push(noted(e.to_string())),
        }
    }
}

enum NormWrite {
    Written,
    Skipped(String),
}

/// Revalidate `row` against the memory as stored now and, if it is still
/// proposed for `step` with the same ref, write that change, then record
/// the replaced values as a revision.
fn write_normalized(
    store: &Store<'_>,
    scope: &Scope<'_>,
    step: Step,
    row: &NormalizeRow,
    record: &Recorder,
) -> crate::db::Result<NormWrite> {
    let Some(current) = queries::get_memory_by_id(store, &row.id)? else {
        return Ok(NormWrite::Skipped("no longer live".into()));
    };
    // Claim rows are written only by their own commands; refuse them as any
    // generic edit does.
    if let Err(refused) = crate::project::refuse_claim_edit(&current) {
        return Ok(NormWrite::Skipped(refused.to_string()));
    }
    let replan = plan_normalize_with(std::slice::from_ref(&current), scope.project, scope.folds);
    let Some(wanted) = step.bucket(&replan).first() else {
        return Ok(NormWrite::Skipped(format!(
            "no longer needs {}",
            step_name(step)
        )));
    };
    if wanted.decision_ref != row.decision_ref {
        return Ok(NormWrite::Skipped(format!(
            "ref changed to {} since planning",
            wanted.decision_ref.as_deref().unwrap_or("none")
        )));
    }
    if wanted.fold != row.fold {
        return Ok(NormWrite::Skipped(format!(
            "project changed to {} since planning",
            current.project.as_deref().unwrap_or("none")
        )));
    }
    let edit = match edit_for(step, &current, wanted)? {
        Ok(edit) => edit,
        Err(why) => return Ok(NormWrite::Skipped(why)),
    };
    let before = Revisions::new(store)
        .current(&current.id)?
        .ok_or(StoreError::NotFound)?;
    Memories::new(store).apply_edit(&current.id, &edit)?;
    record(store, &current.id, &before)?;
    crate::events::emit(
        crate::events::Event::Updated,
        &current.id,
        &current.category,
    );
    Ok(NormWrite::Written)
}

fn step_name(step: Step) -> &'static str {
    match step {
        Step::Respell => "a respell",
        Step::Retype => "a retype",
        Step::AddRef => "a ref",
    }
}

/// The edit `step` makes to `current`; the inner `Err` is a reason to skip.
fn edit_for(
    step: Step,
    current: &Memory,
    wanted: &NormalizeRow,
) -> crate::db::Result<Result<MemoryEdit, String>> {
    let at = MemoryEdit::at(chrono::Utc::now().to_rfc3339());
    if step == Step::Respell {
        let Some(fold) = &wanted.fold else {
            return Ok(Err("no fold matches its project".into()));
        };
        return Ok(Ok(MemoryEdit {
            project: Some(fold.to.clone()),
            ..at
        }));
    }
    let mut metadata = match &current.metadata {
        serde_json::Value::Object(map) => map.clone(),
        serde_json::Value::Null => serde_json::Map::new(),
        _ => {
            return Ok(Err(
                "metadata is not an object, so there is nothing to merge into".into(),
            ))
        }
    };
    match (step, wanted.decision_ref.as_deref()) {
        (_, Some(reference)) => {
            metadata.insert("decision_ref".into(), reference.into());
        }
        (Step::AddRef, None) => return Ok(Err("no ref to write".into())),
        _ => {}
    }
    let metadata = serde_json::Value::Object(metadata);
    let memory_type = match step {
        Step::Retype => Some("decision".to_string()),
        _ => None,
    };
    // The check an update makes, for the type the row will have: a retype
    // without a rationale is refused here (D32).
    let checked_type = memory_type.as_deref().or(current.memory_type.as_deref());
    StructuredFields::default()
        .validate(&metadata, checked_type)
        .map_err(|e| StoreError::Invalid(e.to_string()))?;
    Ok(Ok(MemoryEdit {
        metadata: Some(metadata),
        memory_type,
        ..at
    }))
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
    /// Repeated refs given a letter ([`RefOptions::rename_duplicates`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed: Vec<Renamed>,
    /// The prefix every ref carries, links included ([`RefOptions::prefix`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_prefix: Option<String>,
    /// Refs of rows rejected after their ref was read (a bad Date, no
    /// decision text): still entries of the file, so a row superseding one
    /// waits for it instead of going in unlinked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected_refs: Vec<RejectedRef>,
    /// Refs repeated more often than `--rename-duplicates` can letter
    /// (more than 26 copies): left as they are, so every copy is a conflict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub too_repeated: Vec<String>,
}

/// The ref of a row rejected after it was read, and where it sits. A
/// generated ref is numbered in the same sequence as accepted rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedRef {
    pub line: usize,
    pub decision_ref: String,
}

/// An entry whose ref repeats an earlier one, renamed so both import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Renamed {
    pub line: usize,
    pub from: String,
    pub to: String,
}

/// How a log's refs are read, chosen per log (pack D69, D70).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefOptions {
    /// Put before every ref and every ref a link names, after a space:
    /// `gui-design` reads `D93` as `gui-design D93`. For two logs of one
    /// project that number from the same D1. A row's text that opens by
    /// defining its own bare ref (`D93: ...`) loses that opening, so the
    /// stored content never names a ref other than the prefixed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// For a table with a `Date` column and no ref column: each row's ref is
    /// `<generate>-<date>-<n>`, `n` counting that date's rows from 1 in file
    /// order, struck rows included, so a ref never depends on what else is
    /// imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<String>,
    /// A ref the log repeats keeps its first entry; each later one gets
    /// `b`, `c`, ... and is listed under [`ParsedLog::renamed`]. A renamed
    /// row's text loses an opening definition of the old ref (`D1: ...`),
    /// and links may name the new ref (`Supersedes D1b`). A ref with more
    /// than 26 copies is not renamed. Without it every copy is a conflict
    /// and none imports.
    #[serde(default)]
    pub rename_duplicates: bool,
}

/// Parse a `DECISIONS.md`: markdown tables mapped by header name, and
/// `## D<n>. Title` sections. A ref in both a table and a section is read
/// from the section, which carries the fuller text.
pub fn parse_log(text: &str) -> ParsedLog {
    parse_log_with(text, &RefOptions::default())
}

/// [`parse_log`], reading refs as `options` says.
pub fn parse_log_with(text: &str, options: &RefOptions) -> ParsedLog {
    let lines: Vec<&str> = text.lines().collect();
    let mut log = ParsedLog::default();
    let sections = parse_sections(&lines);
    let mut dated = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_table_start(&lines, i) {
            let generate = options.generate.as_deref();
            i = parse_table(&lines, i, generate, &mut dated, &mut log);
        } else {
            i += 1;
        }
    }
    let in_section = |reference: &str| sections.iter().any(|s| s.decision_ref == reference);
    log.entries.retain(|e| !in_section(&e.decision_ref));
    log.rejected_refs.retain(|r| !in_section(&r.decision_ref));
    log.entries.extend(sections);
    log.entries.sort_by_key(|e| e.line);
    number_dated(&mut log.entries, &mut log.rejected_refs, &dated);
    if options.rename_duplicates {
        log.renamed = rename_duplicates(&mut log);
    }
    if let Some(prefix) = options.prefix.as_deref().map(str::trim) {
        for entry in &mut log.entries {
            entry.title = without_own_definition(&entry.title, &entry.decision_ref);
            entry.decision_ref = prefixed(Some(prefix), &entry.decision_ref);
        }
        for rejected in &mut log.rejected_refs {
            rejected.decision_ref = prefixed(Some(prefix), &rejected.decision_ref);
        }
        for renamed in &mut log.renamed {
            renamed.from = prefixed(Some(prefix), &renamed.from);
            renamed.to = prefixed(Some(prefix), &renamed.to);
        }
        log.ref_prefix = Some(prefix.to_string());
    }
    log
}

/// `title` without an unqualified opening definition of the row's own
/// `reference` (`D1: ...`, `D1 [stated] ...`, `D1 — ...`). Under a prefix the
/// row is stored as `<prefix> D1`, and a content that still defined a bare
/// `D1` would disagree with it, so no link could ever retire the row. A
/// definition of another ref, or one with its own project, is kept as
/// written, and so is a title that would be left empty.
fn without_own_definition(title: &str, reference: &str) -> String {
    let own = matches!(defined_ref(title), Some((None, defined)) if defined == reference);
    let rest = without_ref(title, reference, "");
    match own && !rest.trim().is_empty() {
        true => rest.trim().to_string(),
        false => title.to_string(),
    }
}

/// `reference` with `prefix` before it, when there is one.
fn prefixed(prefix: Option<&str>, reference: &str) -> String {
    match prefix {
        Some(p) => format!("{p} {reference}"),
        None => reference.to_string(),
    }
}

/// Append each dated row's ordinal within its date (rows at the lines in
/// `dated`, whose refs are `<prefix>-<date>` so far), in file order. Rejected
/// rows count in the same sequence, so mending one later never shifts the
/// refs of the rows after it.
fn number_dated(entries: &mut [LogEntry], rejected: &mut [RejectedRef], dated: &[usize]) {
    let mut refs: Vec<(usize, &mut String)> = entries
        .iter_mut()
        .map(|e| (e.line, &mut e.decision_ref))
        .chain(rejected.iter_mut().map(|r| (r.line, &mut r.decision_ref)))
        .filter(|(line, _)| dated.contains(line))
        .collect();
    refs.sort_by_key(|(line, _)| *line);
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (_, reference) in refs {
        let n = seen.entry(reference.clone()).or_default();
        *n += 1;
        *reference = format!("{reference}-{n}");
    }
}

/// Give each repeat of a ref the next letter from `b`, in file order.
/// Rejected rows count in file order like accepted ones, so mending one
/// later never moves a letter from the row after it.
fn rename_duplicates(log: &mut ParsedLog) -> Vec<Renamed> {
    /// Where a ref sits: an accepted entry or a rejected row, by index.
    #[derive(Clone, Copy)]
    enum Slot {
        Entry(usize),
        Rejected(usize),
    }
    let mut slots: Vec<(usize, Slot)> = (log.entries.iter().enumerate())
        .map(|(i, e)| (e.line, Slot::Entry(i)))
        .chain(
            log.rejected_refs
                .iter()
                .enumerate()
                .map(|(i, r)| (r.line, Slot::Rejected(i))),
        )
        .collect();
    slots.sort_by_key(|(line, _)| *line);
    let reference = |log: &ParsedLog, slot: Slot| match slot {
        Slot::Entry(i) => log.entries[i].decision_ref.clone(),
        Slot::Rejected(i) => log.rejected_refs[i].decision_ref.clone(),
    };
    let mut copies: HashMap<String, usize> = HashMap::new();
    for (_, slot) in &slots {
        *copies.entry(reference(log, *slot)).or_default() += 1;
    }
    // More copies than b to z can name: none of the group is renamed, so
    // the repeated-ref check makes every copy a conflict.
    log.too_repeated = copies
        .iter()
        .filter(|(_, n)| **n > MAX_COPIES)
        .map(|(r, _)| r.clone())
        .collect();
    log.too_repeated.sort();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut renamed = Vec::new();
    for (line, slot) in slots {
        let from = reference(log, slot);
        if copies[&from] > MAX_COPIES {
            continue;
        }
        let count = seen.entry(from.clone()).or_default();
        *count += 1;
        if *count == 1 {
            continue;
        }
        let letter = char::from(b'a' + u8::try_from(*count - 1).unwrap_or(25));
        let to = format!("{from}{letter}");
        match slot {
            Slot::Entry(i) => {
                let entry = &mut log.entries[i];
                // The row is stored as `to`: a content still defining
                // `from` would disagree with it.
                entry.title = without_own_definition(&entry.title, &from);
                entry.decision_ref = to.clone();
            }
            Slot::Rejected(i) => log.rejected_refs[i].decision_ref = to.clone(),
        }
        renamed.push(Renamed { line, from, to });
    }
    renamed
}

/// The most copies of one ref `--rename-duplicates` can name: the ref
/// itself and `b` to `z`.
const MAX_COPIES: usize = 26;

/// A calendar day written `YYYY-MM-DD`, the one form the log export reads
/// back ([`crate::decisions_log`]).
fn is_date(text: &str) -> bool {
    crate::decisions_log::is_day(text)
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

/// Parse the table starting at `start`; returns the line after it. With
/// `generate`, a table with a date but no ref column takes `<generate>-<date>`
/// refs, and their lines go into `dated` for [`number_dated`].
fn parse_table(
    lines: &[&str],
    start: usize,
    generate: Option<&str>,
    dated: &mut Vec<usize>,
    log: &mut ParsedLog,
) -> usize {
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
            read_row(&row, &cols, i + 1, lines[i], generate, dated, log);
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
    let text = code_parts(cell)
        .into_iter()
        .map(|(part, code)| {
            if code {
                part.to_string()
            } else {
                part.replace("~~", "").replace("**", "")
            }
        })
        .collect::<String>();
    (text.trim().to_string(), struck)
}

fn read_row(
    row: &[String],
    cols: &Columns,
    line: usize,
    raw: &str,
    generate: Option<&str>,
    dated: &mut Vec<usize>,
    log: &mut ParsedLog,
) {
    let at = |c: Option<usize>| c.and_then(|i| row.get(i)).map(|s| clean(s));
    let unparsed = |reason: &str| Unparsed {
        line,
        reason: reason.to_string(),
        snippet: snippet(raw),
    };
    let generated = || {
        let prefix = generate.filter(|_| cols.reference.is_none())?;
        let (date, _) = at(cols.date).filter(|(d, _)| is_date(d))?;
        Some((format!("{prefix}-{date}"), false))
    };
    let reference = at(cols.reference).filter(|(r, _)| !r.is_empty());
    let from_date = reference.is_none();
    let Some((reference, ref_struck)) = reference.or_else(generated) else {
        log.unparsed.push(unparsed("no ref column"));
        return;
    };
    let decision = at(cols.decision).or_else(|| {
        let i = cols.reference.map_or(1, |r| r + 1);
        row.get(i).map(|s| clean(s))
    });
    if from_date {
        dated.push(line);
    }
    let rejected = |decision_ref: String| RejectedRef { line, decision_ref };
    let Some((title, title_struck)) = decision.filter(|(t, _)| !t.is_empty()) else {
        log.unparsed.push(unparsed("no decision text"));
        log.rejected_refs.push(rejected(reference));
        return;
    };
    let some = |c: Option<usize>| at(c).map(|(t, _)| t).filter(|t| !t.is_empty());
    let date = some(cols.date);
    if date.as_deref().is_some_and(|d| !is_date(d)) {
        log.unparsed.push(unparsed("date is not a YYYY-MM-DD day"));
        log.rejected_refs.push(rejected(reference));
        return;
    }
    log.entries.push(LogEntry {
        decision_ref: reference,
        title,
        rationale: some(cols.why),
        date,
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
    /// What the entry decides, as [`LogEntry::statement`] reads it.
    fn statement(&self) -> String {
        match &self.decision {
            Some(body) => format!("{} {body}", self.title),
            None => self.title.clone(),
        }
    }

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
    /// Repeated refs imported under a letter (`--rename-duplicates`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed: Vec<Renamed>,
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
        renamed: log.renamed.clone(),
        ..Default::default()
    };
    // Superseded decisions count too: an entry the store holds only as a
    // retired decision is already imported, never new again.
    let existing: Vec<(&Memory, String)> = memories
        .iter()
        .filter(|m| m.deleted_at.is_none())
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
            links(entry, log.ref_prefix.as_deref(), &mut plan);
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
    // A rejected row that shares the ref counts: once mended it would make
    // this a conflict, so it is one now, before any copy is stored.
    let twins = log
        .entries
        .iter()
        .map(|e| &e.decision_ref)
        .chain(log.rejected_refs.iter().map(|r| &r.decision_ref))
        .filter(|r| **r == entry.decision_ref)
        .count();
    if log.too_repeated.contains(&entry.decision_ref) {
        let note = "ref repeats more than 26 times: too many to rename";
        plan.conflicts.push(row(None, note));
        return false;
    }
    if twins > 1 {
        plan.conflicts.push(row(None, "ref appears more than once"));
        return false;
    }
    if entry.struck {
        plan.conflicts.push(row(None, "struck through"));
        return false;
    }
    let carrying = |live: bool| -> Vec<&&Memory> {
        existing
            .iter()
            .filter(|(m, r)| *r == entry.decision_ref && m.superseded_by.is_none() == live)
            .map(|(m, _)| m)
            .collect()
    };
    // The live decision is the one to compare; a retired one only when
    // nothing live carries the ref.
    let live = carrying(true);
    let matches = if live.is_empty() {
        carrying(false)
    } else {
        live
    };
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
    // Both sides drop the same validated opening definition, so a row the
    // file writes as `D1: ...` reads as unchanged once stored that way.
    let project = memory.project.as_deref().unwrap_or("");
    let stored = words(without_ref(&memory.content, &entry.decision_ref, project));
    let statement = entry.statement();
    if stored != words(without_ref(&statement, &entry.decision_ref, project)) {
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

/// Sentence punctuation that never changes what a decision says.
const IGNORED: &str = ".,;:";

/// Prose markup removed only as a matched pair, outside literal code spans.
const PAIRED_MARKUP: [&str; 3] = ["**", "__", "~~"];

/// Split prose from matched backtick spans, including multi-backtick fences.
/// Keep the delimiters so parsing/reporting can retain the original code.
/// Unmatched runs stay in prose and are compared as literal symbols.
fn code_parts(text: &str) -> Vec<(&str, bool)> {
    let mut parts = Vec::new();
    let mut prose = 0;
    let mut scan = 0;
    while let Some(open) = text[scan..].find('`').map(|i| scan + i) {
        let escapes = text[..open]
            .bytes()
            .rev()
            .take_while(|b| *b == b'\\')
            .count();
        if escapes % 2 == 1 {
            // An escape consumes only the first backtick. Any remaining run
            // may still open a span. Backslashes inside a span are literal,
            // so closing delimiters below deliberately do not check escapes.
            scan = open + 1;
            continue;
        }
        let width = text[open..].bytes().take_while(|b| *b == b'`').count();
        let mut next = open + width;
        let mut end = None;
        while let Some(close) = text[next..].find('`').map(|i| next + i) {
            let run = text[close..].bytes().take_while(|b| *b == b'`').count();
            next = close + run;
            if run == width {
                end = Some(next);
                break;
            }
        }
        if let Some(end) = end {
            parts.push((&text[prose..open], false));
            parts.push((&text[open..end], true));
            prose = end;
            scan = end;
        } else {
            scan = open + width;
        }
    }
    parts.push((&text[prose..], false));
    parts
}

/// Lowercase words for comparing decisions. Spacing, sentence punctuation
/// and paired prose markup are ignored; code contents compare literally.
/// Every other symbol is a token of
/// its own, so `-1` differs from `1`, `< 100` from `> 100`, `*.rs` from
/// `rs` and `f(x)` from `f x`. A symbol not known to be harmless counts, so
/// doubt lands in "changed", never "unchanged".
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (part, code) in code_parts(text) {
        if code {
            let inner = part.trim_matches('`');
            // A separate token namespace keeps literal case, punctuation and
            // spacing distinct from prose and prevents marker removal in code.
            out.push(format!("\0code:{inner}"));
        } else {
            out.extend(prose_words(part));
        }
    }
    out
}

fn prose_words(text: &str) -> Vec<String> {
    let text = PAIRED_MARKUP
        .iter()
        .fold(text.to_string(), |t, mark| unwrap_pairs(&t, mark));
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

/// `text` with each `mark`…`mark` pair around text replaced by the text. A
/// mark with no partner, or one that wraps only spaces, stays.
fn unwrap_pairs(text: &str, mark: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(mark) {
        let after = &rest[open + mark.len()..];
        let Some(close) = after.find(mark) else {
            break;
        };
        let inner = &after[..close];
        out.push_str(&rest[..open]);
        if inner.is_empty() || inner.starts_with(' ') || inner.ends_with(' ') {
            out.push_str(mark);
            rest = after;
            continue;
        }
        out.push_str(inner);
        rest = &after[close + mark.len()..];
    }
    out.push_str(rest);
    out
}

/// Remove only a validated opening definition with a matching project, before
/// tokenization loses the punctuation that distinguishes definitions from mentions.
fn without_ref<'a>(text: &'a str, reference: &str, project: &str) -> &'a str {
    let Some((qualifier, defined)) = defined_ref(text) else {
        return text;
    };
    if defined != reference
        || qualifier
            .as_deref()
            .is_some_and(|q| !same_project(q, project))
    {
        return text;
    }
    let mut rest = text.trim_start();
    for _ in 0..qualifier
        .as_deref()
        .map_or(0, |q| q.split_whitespace().count())
    {
        let Some((_, tail)) = rest.split_once(char::is_whitespace) else {
            return text;
        };
        rest = tail.trim_start();
    }
    let Some(tail) = rest.strip_prefix(reference) else {
        return text;
    };
    rest = tail.strip_prefix(':').unwrap_or(tail).trim_start();
    for tag in ["[stated]", "[default]"] {
        if let Some(tail) = rest.strip_prefix(tag) {
            rest = tail.strip_prefix(':').unwrap_or(tail).trim_start();
            break;
        }
    }
    for dash in ['-', '—'] {
        if let Some(tail) = rest
            .strip_prefix(dash)
            .filter(|t| t.starts_with(char::is_whitespace))
        {
            rest = tail.trim_start();
            break;
        }
    }
    rest
}

/// Marks that open a quotation.
const QUOTES: [char; 3] = ['"', '“', '`'];

/// Words that turn the link after them around.
const NEGATIONS: [&str; 6] = ["not", "never", "no", "nor", "doesn't", "don't"];

/// `supersedes D<n>` / `confirms D<n>` stated outright in the entry's own
/// decision. Negated, quoted or partial links, and other supersede wording,
/// are ambiguous: reported, never proposed (D34).
fn links(entry: &LogEntry, prefix: Option<&str>, plan: &mut ImportPlan) {
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
        if !matches!(keyword.as_str(), "supersedes" | "confirms") || !is_link_target(target) {
            continue;
        }
        if let Some(why) = hedged(&words, i) {
            doubt = Some(why);
            continue;
        }
        let link = SupersedesRow {
            decision_ref: entry.decision_ref.clone(),
            supersedes: prefixed(prefix, target),
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

// -- import apply -------------------------------------------------------------

/// What [`apply_import`] wrote, and what it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportApplied {
    /// `new` rows now in the store, each with its `memory_id`.
    pub added: Vec<ImportRow>,
    /// Supersedes links carried out: the older decision now points at the
    /// newer one.
    pub linked: Vec<SupersedesRow>,
    /// Links whose target is not a live decision of the project, or not
    /// one only: left for a person.
    pub unlinked: Vec<SupersedesRow>,
    /// Planned rows that no longer qualified when their turn came (the ref
    /// arrived meanwhile); `note` says why. Untouched.
    pub skipped: Vec<ImportRow>,
    /// Rows whose write failed, with the error in `note`. Untouched.
    pub failed: Vec<ImportRow>,
    /// The plan this run applied, planned against the store as it stood.
    pub plan: ImportPlan,
}

/// Plan `log` against the store, then add every `new` entry as a decision
/// of `project` and carry out the supersedes links the file states. Nothing
/// else is written: `changed`, `needs_rationale`, `conflicts` and
/// `ambiguous` rows stay for a person (a row with any ambiguous link
/// wording is not written, even with a plain link beside it), and a stored
/// decision is never overwritten (D30). A second run finds every entry
/// unchanged.
///
/// Rows go in supersession order: an entry that supersedes another entry of
/// the same file is written after it, wherever the two sit. Links that form
/// a cycle refuse the whole import before anything is written.
///
/// Each entry is its own transaction: the recheck that its ref is still
/// absent, the choice of its link targets, its insert and the retiring of
/// those targets. A failed row leaves nothing behind. A link whose target
/// is another entry of the file that this run did not write holds the row
/// back (`skipped`), and so does one whose target the plan did not read as
/// unchanged, or whose stored copy no longer matches the file's entry: the
/// ref may now mean another decision. A link whose target is not exactly one live decision of
/// the project whose refs agree is listed under `unlinked`, never guessed.
///
/// # Errors
///
/// A store error reading the memories. Per-row outcomes are in the result.
pub fn apply_import(
    store: &Store<'_>,
    log: &ParsedLog,
    project: &str,
    source: &str,
) -> crate::db::Result<ImportApplied> {
    let plan = plan_import(log, project, source, &live_decisions(store)?);
    Ok(apply_plan(store, log, plan, &|tx, add, targets| {
        add.write(tx, targets)
    }))
}

/// Writes a prepared row and retires its targets, inside the row's
/// transaction: [`PreparedAdd::write`], or a stand-in in tests.
type WriteRow<'f> = dyn Fn(&Store<'_>, &PreparedAdd, &[String]) -> crate::db::Result<()> + 'f;

/// What every row of one import shares.
struct ImportBatch<'a> {
    project: &'a str,
    source: &'a str,
    /// Every ref the file defines: a link to one of these waits for it.
    in_file: HashSet<&'a str>,
    /// The refs of rows the parse rejected, also in `in_file`: named in
    /// the reason a row superseding one is held back.
    rejected: HashSet<&'a str>,
    /// The file's parsed entries by ref, to recheck a target against.
    entries: HashMap<&'a str, &'a LogEntry>,
    /// The in-file refs a link may retire: those the plan read as
    /// unchanged against one stored decision, and those this run wrote.
    retirable: HashSet<String>,
}

/// Write `plan`'s `new` rows, each in its own transaction (see
/// [`apply_import`]).
fn apply_plan(
    store: &Store<'_>,
    log: &ParsedLog,
    plan: ImportPlan,
    write: &WriteRow<'_>,
) -> ImportApplied {
    let mut applied = ImportApplied::default();
    let mut batch = ImportBatch {
        project: &plan.project,
        source: &plan.source,
        in_file: log
            .entries
            .iter()
            .map(|e| e.decision_ref.as_str())
            .chain(log.rejected_refs.iter().map(|r| r.decision_ref.as_str()))
            .collect(),
        rejected: log
            .rejected_refs
            .iter()
            .map(|r| r.decision_ref.as_str())
            .collect(),
        entries: log
            .entries
            .iter()
            .map(|e| (e.decision_ref.as_str(), e))
            .collect(),
        retirable: plan
            .unchanged
            .iter()
            .map(|r| r.decision_ref.clone())
            .collect(),
    };
    let ambiguous: HashSet<&str> = plan
        .ambiguous
        .iter()
        .map(|r| r.decision_ref.as_str())
        .collect();
    let rows: Vec<&ImportRow> = plan
        .new
        .iter()
        .filter(|r| !ambiguous.contains(r.decision_ref.as_str()))
        .collect();
    let Some(order) = supersession_order(&rows, &plan.supersedes) else {
        applied.skipped = rows
            .iter()
            .map(|r| ImportRow {
                note: "supersedes links form a cycle: nothing written".into(),
                ..(*r).clone()
            })
            .collect();
        applied.plan = plan;
        return applied;
    };
    for row in order {
        let links = links_of(&plan.supersedes, &row.decision_ref);
        let confirms: Vec<String> = links_of(&plan.confirms, &row.decision_ref)
            .into_iter()
            .map(|l| l.supersedes)
            .collect();
        let written = store.atomically(|s| write_row(s, &batch, row, &links, confirms, write));
        let noted = |note: String| ImportRow {
            note,
            ..row.clone()
        };
        match written {
            Ok(RowWrite::Added { add, unlinked }) => {
                // Committed: what follows the write cannot undo it, so a
                // failure there is reported on the row, not as a failed row.
                let note = match add.finish(store) {
                    Ok(_) => row.note.clone(),
                    Err(e) => format!("added; embedding or indexing after the write failed: {e}"),
                };
                applied
                    .linked
                    .extend(links.into_iter().filter(|l| !unlinked.contains(l)));
                applied.unlinked.extend(unlinked);
                batch.retirable.insert(row.decision_ref.clone());
                applied.added.push(ImportRow {
                    memory_id: Some(add.id().to_string()),
                    note,
                    ..row.clone()
                });
            }
            Ok(RowWrite::Skipped(note)) => applied.skipped.push(noted(note)),
            Err(e) => applied.failed.push(noted(e.to_string())),
        }
    }
    applied.plan = plan;
    applied
}

/// The links `reference` states, each target once: an entry may name the
/// same target twice (in its title and its body), and retiring a target
/// twice would refuse the second and roll the row back.
fn links_of(links: &[SupersedesRow], reference: &str) -> Vec<SupersedesRow> {
    let mut seen = HashSet::new();
    links
        .iter()
        .filter(|l| l.decision_ref == reference)
        .filter(|l| seen.insert(l.supersedes.clone()))
        .cloned()
        .collect()
}

/// `rows` in file order, except that a row comes after every row of the
/// batch it supersedes. `None` when the links form a cycle.
fn supersession_order<'a>(
    rows: &[&'a ImportRow],
    links: &[SupersedesRow],
) -> Option<Vec<&'a ImportRow>> {
    let batch: HashSet<&str> = rows.iter().map(|r| r.decision_ref.as_str()).collect();
    let ready = |row: &ImportRow, placed: &HashSet<&str>| {
        links
            .iter()
            .filter(|l| l.decision_ref == row.decision_ref)
            .filter(|l| batch.contains(l.supersedes.as_str()))
            .all(|l| placed.contains(l.supersedes.as_str()))
    };
    let mut waiting = rows.to_vec();
    let mut placed: HashSet<&str> = HashSet::new();
    let mut order = Vec::with_capacity(rows.len());
    while !waiting.is_empty() {
        let at = waiting.iter().position(|r| ready(r, &placed))?;
        let row = waiting.remove(at);
        placed.insert(row.decision_ref.as_str());
        order.push(row);
    }
    Some(order)
}

/// What one row's transaction did.
enum RowWrite {
    Added {
        add: Box<PreparedAdd>,
        unlinked: Vec<SupersedesRow>,
    },
    Skipped(String),
}

/// One row, inside its transaction: recheck, pick targets, add, retire.
fn write_row(
    store: &Store<'_>,
    batch: &ImportBatch<'_>,
    row: &ImportRow,
    links: &[SupersedesRow],
    confirms: Vec<String>,
    write: &WriteRow<'_>,
) -> crate::db::Result<RowWrite> {
    let stored = live_decisions(store)?;
    let held = project_refs(&stored, batch.project, Retired::Include);
    if held.contains_key(&row.decision_ref) {
        return Ok(RowWrite::Skipped("already in the store".into()));
    }
    let pending = links.iter().find(|l| {
        batch.in_file.contains(l.supersedes.as_str()) && !held.contains_key(&l.supersedes)
    });
    if let Some(link) = pending {
        let why = match batch.rejected.contains(link.supersedes.as_str()) {
            true => "an entry of this file rejected as unparsed",
            false => "an entry of this file not in the store",
        };
        return Ok(RowWrite::Skipped(format!(
            "supersedes {}, {why}",
            link.supersedes
        )));
    }
    let live = project_refs(&stored, batch.project, Retired::Exclude);
    if let Some(link) = links
        .iter()
        .find(|l| !matches_file(batch, &live, &l.supersedes))
    {
        return Ok(RowWrite::Skipped(format!(
            "supersedes {}, whose stored copy does not match the file",
            link.supersedes
        )));
    }
    let (targets, unlinked) = link_targets(links, &live);
    let add = prepare_imported(batch, row, confirms)?;
    write(store, &add, &targets)?;
    Ok(RowWrite::Added {
        add: Box::new(add),
        unlinked,
    })
}

/// Whether a link may retire `target`: always for a ref outside the file;
/// for an entry of the file, only when it is retirable and exactly one live
/// decision carries it with the file's text and rationale, so a stored
/// copy the file now means differently is never retired.
fn matches_file(
    batch: &ImportBatch<'_>,
    live: &HashMap<String, Vec<&Memory>>,
    target: &str,
) -> bool {
    if !batch.in_file.contains(target) {
        return true;
    }
    if !batch.retirable.contains(target) {
        return false;
    }
    let Some(entry) = batch.entries.get(target) else {
        return false;
    };
    match live.get(target).map(Vec::as_slice) {
        Some([one]) => difference(one, entry).is_none(),
        _ => false,
    }
}

fn live_decisions(store: &Store<'_>) -> crate::db::Result<Vec<Memory>> {
    Memories::new(store).live_of_type("decision")
}

/// Whether [`project_refs`] counts superseded decisions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retired {
    Include,
    Exclude,
}

/// Each ref `project`'s decisions carry, with the memories carrying it.
fn project_refs<'m>(
    memories: &'m [Memory],
    project: &str,
    retired: Retired,
) -> HashMap<String, Vec<&'m Memory>> {
    let mut refs: HashMap<String, Vec<&Memory>> = HashMap::new();
    let ours = memories
        .iter()
        .filter(|m| m.deleted_at.is_none())
        .filter(|m| retired == Retired::Include || m.superseded_by.is_none())
        .filter(|m| m.memory_type.as_deref() == Some("decision"))
        .filter(|m| {
            m.project
                .as_deref()
                .is_some_and(|p| same_project(p, project))
        });
    for memory in ours {
        if let Some((r, _)) = carried_ref(memory) {
            refs.entry(r).or_default().push(memory);
        }
    }
    refs
}

/// Whether a memory's refs agree, project qualifiers included, as
/// normalize requires before trusting one ([`ref_disagreement`]).
fn identity_agrees(memory: &Memory) -> bool {
    let defined = defined_ref(&memory.content);
    if let Some((Some(named), _)) = &defined {
        let field = memory.project.as_deref().unwrap_or("");
        if field.is_empty() || !same_project(named, field) {
            return false;
        }
    }
    ref_disagreement(memory, defined.as_ref().map(|(_, r)| r.as_str())).is_none()
}

/// The memory ids `links` retire, and the links whose target is not exactly
/// one live decision whose refs agree.
fn link_targets(
    links: &[SupersedesRow],
    live: &HashMap<String, Vec<&Memory>>,
) -> (Vec<String>, Vec<SupersedesRow>) {
    let mut targets = Vec::new();
    let mut unlinked = Vec::new();
    for link in links {
        match live.get(&link.supersedes).map(Vec::as_slice) {
            Some([one]) if identity_agrees(one) => {
                if !targets.contains(&one.id) {
                    targets.push(one.id.clone());
                }
            }
            _ => unlinked.push(link.clone()),
        }
    }
    (targets, unlinked)
}

/// `row` as a decision of the batch's project, ready to write. Its text is
/// the entry's statement, so a re-plan reads it as unchanged; the ref, why,
/// date, tag and where it came from go in `metadata`. No review date is
/// set: these were decided before they were written down (pack D74).
fn prepare_imported(
    batch: &ImportBatch<'_>,
    row: &ImportRow,
    confirms: Vec<String>,
) -> crate::db::Result<PreparedAdd> {
    let mut metadata = serde_json::json!({
        "decision_ref": row.decision_ref,
        "rationale": row.rationale,
        "imported": { "source": batch.source, "line": row.line },
    });
    for (key, value) in [("date", &row.date), ("tag", &row.tag)] {
        if let Some(value) = value {
            metadata[key] = value.clone().into();
        }
    }
    if !confirms.is_empty() {
        metadata["confirms"] = confirms.into();
    }
    let input = crate::models::MemoryAddInput {
        content: row.statement(),
        category: "decision".into(),
        source: IMPORT_SOURCE.into(),
        metadata,
        project: Some(batch.project.to_string()),
        ..Default::default()
    };
    let fields = StructuredFields {
        memory_type: Some("decision".into()),
        ..Default::default()
    };
    PreparedAdd::new(input, &fields, queries::Review::Unscheduled)
}

/// `source` an imported decision is stored with.
pub const IMPORT_SOURCE: &str = "decisions-import";

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

/// What an apply run wrote, as markdown: the rows it updated, the rows it
/// could not, and the buckets it leaves for a person.
pub fn render_normalize_applied(applied: &NormalizeApplied) -> String {
    let plan = &applied.plan;
    let scope = applied.project.as_deref().unwrap_or("all projects");
    let mut out = format!(
        "# Decisions normalise applied: {scope}\n\nRenamed the project of {} memories, typed {} as decisions, wrote metadata.decision_ref on {}; {} skipped as changed since planning, {} failed. Left unchanged: {} need rationale, {} conflicts, {} mentions.\n",
        applied.respelled.len(),
        applied.retyped.len(),
        applied.updated.len(),
        applied.skipped.len(),
        applied.failed.len(),
        plan.needs_rationale.len(),
        plan.conflicts.len(),
        plan.mentions.len()
    );
    section(&mut out, "Respelled", &applied.respelled, norm_line);
    section(&mut out, "Retyped", &applied.retyped, norm_line);
    section(&mut out, "Updated", &applied.updated, norm_line);
    section(&mut out, "Skipped", &applied.skipped, norm_line);
    section(&mut out, "Failed", &applied.failed, norm_line);
    out
}

/// The normalise plan as markdown: counts, then every row per bucket.
pub fn render_normalize(plan: &NormalizePlan) -> String {
    let scope = plan.project.as_deref().unwrap_or("all projects");
    let mut out = format!(
        "# Decisions normalise dry run: {scope}\n\nNothing was written to the store. Proposed: {} respell, {} retype, {} add ref. Needs a person: {} need rationale, {} conflicts. Report only: {} mentions.\n",
        plan.respell.len(),
        plan.retype.len(),
        plan.add_ref.len(),
        plan.needs_rationale.len(),
        plan.conflicts.len(),
        plan.mentions.len()
    );
    section(&mut out, "Respell project", &plan.respell, norm_line);
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

/// `text` on one line, so it stays inside its list item. Multiline values use
/// a quoted Rust string literal: escapes preserve every character, and an
/// inline delimiter longer than any embedded run keeps fences inert.
fn one_line(text: &str) -> String {
    if text.contains(['\n', '\r']) {
        let escaped = format!("{text:?}");
        let width = escaped.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1;
        let delimiter = "`".repeat(width);
        return format!("escaped string {delimiter} {escaped} {delimiter}");
    }
    let mut out = String::new();
    for (part, code) in code_parts(text) {
        if code {
            out.push_str(part);
        } else {
            for c in part.chars() {
                if c.is_whitespace() {
                    if !out.ends_with(' ') {
                        out.push(' ');
                    }
                } else {
                    out.push(c);
                }
            }
        }
    }
    out.trim().to_string()
}

fn link_line(r: &SupersedesRow) -> String {
    format!("{} -> {} (line {})", r.decision_ref, r.supersedes, r.line)
}

/// The import plan as markdown: counts, then every row per bucket.
pub fn render_import(plan: &ImportPlan) -> String {
    let mut out = format!(
        "# Decisions import dry run: {} from {}\n\nNothing was written to the store. New {}, unchanged {}, changed {}, needs rationale {}, conflicts {}, supersedes {}, confirms {}, ambiguous {}, unparsed {}.\n",
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
    if !plan.renamed.is_empty() {
        section(&mut out, "Renamed repeats", &plan.renamed, |r| {
            format!("line {}: {} -> {}", r.line, r.from, r.to)
        });
    }
    section(&mut out, "Unparsed", &plan.unparsed, |u| {
        format!("line {} — {}: {}", u.line, u.reason, u.snippet)
    });
    out
}

/// What an import apply wrote, as markdown: the decisions it added and the
/// links it made, then what it could not, then the buckets it leaves for a
/// person.
pub fn render_import_applied(applied: &ImportApplied) -> String {
    let plan = &applied.plan;
    let mut out = format!(
        "# Decisions import applied: {} from {}\n\nAdded {} decisions and made {} supersedes links; {} links left unmade, {} skipped as already stored, {} failed. Left for a person: {} changed, {} need rationale, {} conflicts, {} ambiguous, {} unparsed.\n",
        plan.project,
        plan.source,
        applied.added.len(),
        applied.linked.len(),
        applied.unlinked.len(),
        applied.skipped.len(),
        applied.failed.len(),
        plan.changed.len(),
        plan.needs_rationale.len(),
        plan.conflicts.len(),
        plan.ambiguous.len(),
        plan.unparsed.len()
    );
    section(&mut out, "Added", &applied.added, import_line);
    section(&mut out, "Linked", &applied.linked, link_line);
    section(&mut out, "Unlinked", &applied.unlinked, link_line);
    section(&mut out, "Skipped", &applied.skipped, import_line);
    section(&mut out, "Failed", &applied.failed, import_line);
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
        assert_eq!(defined_ref("Supersedes D1: New policy"), None);
        assert_eq!(defined_ref("Confirms D7 (still holds)"), None);
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

    #[test]
    fn code_openers_respect_escape_parity_and_exact_run_widths() {
        for text in [r"\`foo\`", r"\\\`foo\`", "``foo```"] {
            assert_eq!(code_parts(text), [(text, false)], "{text}");
        }
        for (text, prose, code) in [
            (r"\\`foo`", r"\\", "`foo`"),
            (r"\\``foo``", r"\\", "``foo``"),
            (r"\``foo`", r"\`", "`foo`"),
            (r"\```foo``", r"\`", "``foo``"),
            (r"`foo\`", "", r"`foo\`"),
            ("``a`b``", "", "``a`b``"),
        ] {
            assert_eq!(
                code_parts(text),
                [(prose, false), (code, true), ("", false)],
                "{text}"
            );
        }
        assert_ne!(words(r"\`foo\`"), words(r"\``foo\``"));
        assert_eq!(words(r"\\`foo`"), words(r"\\``foo``"));
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::Database;

    fn decision(id: &str, reference: &str) -> NewMemory {
        let mut row = NewMemory::new(id, "x", "2026-11-01T00:00:00Z");
        row.memory_type = "decision".into();
        row.category = "decision".into();
        row.project = Some("mill".into());
        row.metadata = serde_json::json!({"decision_id": reference, "rationale": "x"});
        row
    }

    /// The revision is the last write in a row's transaction. When it
    /// fails, after the row edit has landed in the page, the page rolls the
    /// row back, no revision is left, and the next row is still written
    /// with its revision.
    #[test]
    fn a_failed_revision_rolls_back_its_row_and_the_next_row_goes_through() {
        let db = Database::open_in_memory().expect("database");
        let store = db.store();
        let memories = Memories::new(&store);
        memories.insert(&decision("m-bad", "D1")).expect("insert");
        memories.insert(&decision("m-ok", "D2")).expect("insert");
        let plan = plan_normalize(&memories.all_live().expect("read"), None);

        let record = |s: &Store<'_>, id: &str, before: &Tracked| {
            if id == "m-bad" {
                // The row edit is already in the page when this runs.
                let edited = queries::get_memory_by_id(s, id)?.ok_or(StoreError::NotFound)?;
                assert_eq!(edited.metadata["decision_ref"], "D1");
                return Err(StoreError::Engine("revision write failed".into()));
            }
            record_revision(s, id, before)
        };
        let mut applied = NormalizeApplied::default();
        let folds = Folds::default();
        let scope = Scope {
            project: None,
            folds: &folds,
        };
        apply_rows(
            &store,
            &scope,
            Step::AddRef,
            &plan.add_ref,
            &record,
            &mut applied,
        );

        let ids = |rows: &[NormalizeRow]| rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&applied.failed), ["m-bad"]);
        assert_eq!(ids(&applied.updated), ["m-ok"]);
        let read = |id: &str| {
            queries::get_memory_by_id(&store, id)
                .expect("read")
                .expect("live")
        };
        assert_eq!(read("m-bad").metadata.get("decision_ref"), None);
        assert!(Revisions::new(&store)
            .list("m-bad", 10)
            .expect("history")
            .is_empty());
        assert_eq!(read("m-ok").metadata["decision_ref"], "D2");
        assert_eq!(
            Revisions::new(&store)
                .list("m-ok", 10)
                .expect("history")
                .len(),
            1
        );
    }

    /// A row planned under one fold whose project is edited before the
    /// write to a spelling another fold of the set renames is skipped: the
    /// write compares the planned fold, not just "some fold matches".
    #[test]
    fn a_respell_skips_a_row_whose_project_moved_to_another_fold() {
        let db = Database::open_in_memory().expect("database");
        let store = db.store();
        let memories = Memories::new(&store);
        let mut row = NewMemory::new("m-1", "worklog", "2026-11-01T00:00:00Z");
        row.project = Some("a_b".into());
        memories.insert(&row).expect("insert");
        let folds = Folds::new(vec![
            Fold::parse("a_b=a-b").expect("fold"),
            Fold::parse("A_B=a-b").expect("fold"),
        ])
        .expect("folds");
        let plan = plan_normalize_with(&memories.all_live().expect("read"), None, &folds);
        assert_eq!(plan.respell.len(), 1);
        assert_eq!(
            plan.respell[0].fold.as_ref().map(|f| f.from.as_str()),
            Some("a_b")
        );

        // After planning, before the write: another writer moves the row.
        let edit = MemoryEdit {
            project: Some("A_B".into()),
            ..MemoryEdit::at("2026-11-02T00:00:00Z")
        };
        memories.apply_edit("m-1", &edit).expect("edit");

        let mut applied = NormalizeApplied::default();
        let scope = Scope {
            project: None,
            folds: &folds,
        };
        apply_rows(
            &store,
            &scope,
            Step::Respell,
            &plan.respell,
            &record_revision,
            &mut applied,
        );
        assert!(applied.respelled.is_empty());
        assert_eq!(applied.skipped.len(), 1);
        assert!(
            applied.skipped[0].note.contains("A_B"),
            "{}",
            applied.skipped[0].note
        );
        let read = queries::get_memory_by_id(&store, "m-1")
            .expect("read")
            .expect("live");
        assert_eq!(
            read.project.as_deref(),
            Some("A_B"),
            "the newer edit stands"
        );
        assert!(Revisions::new(&store)
            .list("m-1", 10)
            .expect("history")
            .is_empty());
    }

    const LOG: &str = "| # | Decision | Why |\n|---|---|---|\n| D1 | Old | x |\n| D2 | Supersedes D1. New | y |\n";

    fn ids_of(rows: &[ImportRow]) -> Vec<&str> {
        rows.iter().map(|r| r.decision_ref.as_str()).collect()
    }

    /// A row whose write fails after its insert and retirement are in the
    /// page leaves neither behind: its target stays live and no memory of
    /// it is stored.
    #[test]
    fn a_failed_row_write_rolls_back_its_insert_and_retirement() {
        let db = Database::open_in_memory().expect("database");
        let store = db.store();
        let log = parse_log(LOG);
        let plan = plan_import(&log, "mill", "d.md", &[]);
        let write = |tx: &Store<'_>, add: &PreparedAdd, targets: &[String]| {
            add.write(tx, targets)?;
            if targets.is_empty() {
                return Ok(());
            }
            Err(StoreError::Engine("write failed after the retire".into()))
        };
        let applied = apply_plan(&store, &log, plan, &write);

        assert_eq!(ids_of(&applied.added), ["D1"]);
        assert_eq!(ids_of(&applied.failed), ["D2"]);
        assert!(applied.linked.is_empty());
        let live = Memories::new(&store).all_live().expect("read");
        assert_eq!(live.len(), 1, "D2 left nothing behind");
        assert!(live[0].superseded_by.is_none(), "D1 is not retired");
    }

    /// A write that lands between planning and a row's turn is seen by
    /// the row's own transaction: the ref is not added twice, and a row
    /// superseding it is held back, since this run neither read nor wrote
    /// the decision now under that ref.
    #[test]
    fn a_ref_written_after_planning_is_not_added_again() {
        let db = Database::open_in_memory().expect("database");
        let store = db.store();
        let log = parse_log(LOG);
        let plan = plan_import(&log, "mill", "d.md", &[]);
        assert_eq!(ids_of(&plan.new), ["D1", "D2"]);
        Memories::new(&store)
            .insert(&decision("m-elsewhere", "D1"))
            .expect("insert");

        let applied = apply_plan(&store, &log, plan, &|tx, add, targets| {
            add.write(tx, targets)
        });

        assert_eq!(ids_of(&applied.skipped), ["D1", "D2"]);
        assert!(applied.skipped[1].note.contains("D1"));
        assert!(applied.added.is_empty());
        let held = Memories::new(&store)
            .get_live("m-elsewhere")
            .expect("read")
            .expect("live");
        assert!(
            held.superseded_by.is_none(),
            "the D1 that is there stays live"
        );
    }
}
