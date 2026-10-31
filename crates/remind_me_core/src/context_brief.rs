//! The brief: what is worth knowing about a project, within a token budget.
//!
//! One builder, three callers, one [`Mode`] each:
//!
//! - [`Mode::Session`]: the SessionStart hook. Persona, reminders due within
//!   a week and the project's decisions due for a follow-up review
//!   ([`crate::review`]; listed there, not again below), then the project's
//!   sections. An unknown project leaves
//!   the project sections empty: the hook names whatever directory it is in.
//! - [`Mode::Project`]: `remind_me_brief`, `GET /api/brief` and
//!   `rusty-remind-me brief`. The project's sections only. An unknown project
//!   is refused with the closest known names, never guessed.
//! - [`Mode::Prompt`]: the per-prompt hook. Only the best matches for the
//!   prompt, so it does not repeat the session's brief.
//!
//! The project's sections, in priority order:
//!
//! 1. **state**: the newest session summaries. Not windowed, so a dormant
//!    project still has a state.
//! 2. **open**: blockers raised in the window, then decisions and action
//!    items with no outcome. Open is open, so those two are not windowed.
//! 3. **activity**: everything else in the window, newest first, the current
//!    branch's first.
//!
//! # Scope
//!
//! A project name resolves through the entity graph, so an alias finds its
//! entity. A memory is in scope when it is linked to that entity, or when one
//! of the names (input, canonical, aliases) equals its `project`, the
//! repository of its `git_remote`, one of its tags, or its subject or object.
//! Every comparison is on [`normalize_entity_name`].
//!
//! # Budget
//!
//! Tokens of the returned text, by the estimator search trims with
//! ([`crate::retrieval::estimated_content_tokens`], bytes / 4). Every piece is costed as the
//! larger of its compact JSON and its Markdown, rounded up, so either
//! rendering of the brief stays within the budget: the envelope and each
//! section's heading are reserved first, then each section is allotted a
//! share of what is left by weight, so what one section leaves unused rolls
//! forward. Items are dropped whole, never cut, and counted in `omitted`.
//! The returned text never exceeds the budget: when the envelope and headings
//! alone do not fit, aliases are cut from the end (`aliases_omitted` counts
//! them), and if that is not enough the brief is refused with the budget it
//! needs.

use crate::db::entities::Entities;
use crate::db::memories::Memories;
use crate::db::{Result, Store, StoreError};
use crate::entity::{normalize_entity_name, resolve_entity};
use crate::models::{Memory, MemorySearchInput, ReminderWindow};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};

/// The window when none is given.
pub const DEFAULT_SINCE: &str = "7d";
/// The longest window accepted.
pub const MAX_WINDOW_DAYS: i64 = 365;
/// `remind_me_brief`'s budget when none is given, and the accepted range.
pub const DEFAULT_TOKEN_BUDGET: usize = 1500;
pub const MIN_TOKEN_BUDGET: usize = 200;
pub const MAX_TOKEN_BUDGET: usize = 8000;
/// Days ahead a reminder counts as due.
pub const REMINDER_DAYS: i64 = 7;

const PERSONA_LIMIT: usize = 10;
const STATE_LIMIT: usize = 3;
const OPEN_LIMIT: usize = 15;
const ACTIVITY_LIMIT: usize = 20;
const HIT_LIMIT: usize = 5;
const REVIEW_LIMIT: usize = 5;
const SUGGESTION_LIMIT: usize = 5;
/// An item's text is its first line, cut to this many characters.
const TEXT_CHARS: usize = 240;

/// Which caller the brief is for. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Session,
    Project,
    Prompt,
}

/// What to build a brief for.
#[derive(Debug, Clone, Default)]
pub struct ContextRequest<'a> {
    pub mode: Mode,
    /// A project or entity name, or an alias. `None` (Session only) is every
    /// project.
    pub project: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub prompt: Option<&'a str>,
    /// `7d`, `12h` or RFC 3339; `None` is [`DEFAULT_SINCE`].
    pub since: Option<&'a str>,
    /// Tokens; `0` means unlimited.
    pub budget: usize,
}

/// One memory, as the brief shows it. Drill down with `remind_me_get(id)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    /// When it was written; for a reminder, when it is due.
    pub at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub name: &'static str,
    pub items: Vec<Item>,
    /// Items that did not fit the budget.
    pub omitted: usize,
}

/// The project a name resolved to, and what matched it.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectMatch {
    pub name: String,
    pub entity_id: Option<String>,
    pub aliases: Vec<String>,
    /// Aliases cut to fit the budget.
    pub aliases_omitted: usize,
    /// Sorted: `alias`, `entity`, `git_remote`, `project`, `tag`.
    pub matched_by: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Budget {
    /// `0` means unlimited.
    pub limit: usize,
    pub used: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Brief {
    pub project: Option<ProjectMatch>,
    /// Start of the window, RFC 3339.
    pub since: String,
    pub sections: Vec<Section>,
    pub budget: Budget,
    #[serde(skip)]
    pub mode: Mode,
    #[serde(skip)]
    pub branch: Option<String>,
}

/// What [`build`] produced.
#[derive(Debug, Clone)]
pub enum Outcome {
    Brief(Brief),
    /// [`Mode::Project`] only: nothing is known by that name.
    UnknownProject {
        name: String,
        suggestions: Vec<String>,
    },
}

/// Parse a window start: `<n>d`, `<n>h`, RFC 3339 or a date phrase (its
/// start: `last week` is last Monday), at most [`MAX_WINDOW_DAYS`] back and
/// not in the future.
pub fn parse_since(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    parse_since_with(raw, now, crate::date_phrase::configured_zone)
}

/// [`parse_since`] with the zone injected; asked for only by a phrase.
fn parse_since_with(
    raw: &str,
    now: DateTime<Utc>,
    zone: impl Fn() -> std::result::Result<chrono_tz::Tz, String>,
) -> Result<DateTime<Utc>> {
    let raw = raw.trim();
    let invalid = || {
        StoreError::Invalid(format!(
            "`since` must be like `7d`, `12h`, an RFC 3339 time or a date phrase like \
             `yesterday` or `last week`, at most {MAX_WINDOW_DAYS} days back; got {raw:?}"
        ))
    };
    let relative = |unit: fn(i64) -> Option<Duration>, n: &str| {
        n.parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .and_then(unit)
            .and_then(|span| now.checked_sub_signed(span))
    };
    let fixed = if let Some(n) = raw.strip_suffix('d') {
        relative(Duration::try_days, n)
    } else if let Some(n) = raw.strip_suffix('h') {
        relative(Duration::try_hours, n)
    } else {
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    };
    let start = match fixed {
        Some(at) => at,
        None => {
            let tz = zone().map_err(StoreError::Invalid)?;
            crate::date_phrase::parse(raw, now.with_timezone(&tz))
                .ok_or_else(invalid)?
                .start
        }
    };
    if start > now || now - start > Duration::days(MAX_WINDOW_DAYS) {
        return Err(invalid());
    }
    Ok(start)
}

/// What to tell a caller who named an unknown project: the closest known
/// names to send instead, or how to find one.
pub fn unknown_project_message(name: &str, suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        format!("no project or entity named {name:?}; find its name with remind_me_search")
    } else {
        format!(
            "no project or entity named {name:?}; did you mean: {}?",
            suggestions.join(", ")
        )
    }
}

/// Check a [`Mode::Project`] budget is in range.
pub fn check_token_budget(budget: usize) -> Result<()> {
    if (MIN_TOKEN_BUDGET..=MAX_TOKEN_BUDGET).contains(&budget) {
        return Ok(());
    }
    Err(StoreError::Invalid(format!(
        "`token_budget` must be between {MIN_TOKEN_BUDGET} and {MAX_TOKEN_BUDGET}; got {budget}"
    )))
}

/// Build the brief for `request`.
pub fn build(store: &Store<'_>, request: &ContextRequest<'_>) -> Result<Outcome> {
    let now = Utc::now();
    let since = parse_since(request.since.unwrap_or(DEFAULT_SINCE), now)?;
    let mut brief = Brief {
        project: None,
        since: since.to_rfc3339(),
        sections: Vec::new(),
        budget: Budget {
            limit: request.budget,
            used: 0,
        },
        mode: request.mode,
        branch: request.branch.map(str::to_string),
    };
    let prompt = request.prompt.map(str::trim).filter(|p| !p.is_empty());
    if request.mode == Mode::Prompt {
        let drafts = match prompt {
            Some(prompt) => vec![hits_draft(store, prompt, now)?],
            None => Vec::new(),
        };
        fit(&mut brief, drafts)?;
        return Ok(Outcome::Brief(brief));
    }

    // Only what may be shown takes part in discovery too: a name that lives
    // only on hidden rows (sensitive, superseded, expired, dialog) is neither
    // known nor suggested.
    let memories: Vec<Memory> = Memories::new(store)
        .all_live()?
        .into_iter()
        .filter(|m| injectable(m, now))
        .collect();
    let scope = match request.project.map(str::trim).filter(|p| !p.is_empty()) {
        None if request.mode == Mode::Project => {
            return Err(StoreError::Invalid(
                "`project` must name a project or entity".to_string(),
            ));
        }
        None => Scope::everything(),
        Some(name) => match resolve_scope(store, name, &memories)? {
            Some(scope) => scope,
            None if request.mode == Mode::Project => {
                return Ok(Outcome::UnknownProject {
                    name: name.to_string(),
                    suggestions: suggestions(store, name, &memories)?,
                });
            }
            None => Scope::nothing(),
        },
    };

    let in_scope: Vec<&Memory> = memories.iter().filter(|m| scope.admits(m)).collect();
    brief.project = scope.project_match(&in_scope);

    let mut drafts = Vec::new();
    let mut rest = in_scope;
    if request.mode == Mode::Session {
        drafts.push(persona_draft(store)?);
        drafts.push(reminders_draft(store, now)?);
        let reviews = reviews_draft(&memories, &scope, now);
        let listed: HashSet<&str> = reviews.items.iter().map(|i| i.id.as_str()).collect();
        rest.retain(|m| !listed.contains(m.id.as_str()));
        drafts.push(reviews);
    }
    drafts.extend(project_drafts(&rest, since, request.branch));
    if request.mode == Mode::Session {
        if let Some(prompt) = prompt {
            drafts.push(hits_draft(store, prompt, now)?);
        }
    }
    fit(&mut brief, drafts)?;
    Ok(Outcome::Brief(brief))
}

// -- scope --------------------------------------------------------------------

/// The memories a project name covers.
struct Scope {
    /// `None` admits everything.
    names: Option<HashSet<String>>,
    linked: HashSet<String>,
    project: Option<ProjectMatch>,
}

impl Scope {
    fn everything() -> Self {
        Self {
            names: None,
            linked: HashSet::new(),
            project: None,
        }
    }

    fn nothing() -> Self {
        Self {
            names: Some(HashSet::new()),
            linked: HashSet::new(),
            project: None,
        }
    }

    /// Why `memory` is in scope, or `None` when it is not.
    fn reason(&self, memory: &Memory) -> Option<&'static str> {
        let names = self.names.as_ref()?;
        let has = |v: Option<&str>| v.is_some_and(|v| names.contains(&normalize_entity_name(v)));
        if self.linked.contains(&memory.id) {
            Some("entity")
        } else if has(memory.project.as_deref()) {
            Some("project")
        } else if has(memory.git_remote.as_deref().and_then(repo_of)) {
            Some("git_remote")
        } else if memory.tags.iter().any(|t| has(Some(t.as_str()))) {
            Some("tag")
        } else if has(memory.subject.as_deref()) || has(memory.object.as_deref()) {
            Some("entity")
        } else {
            None
        }
    }

    fn admits(&self, memory: &Memory) -> bool {
        self.names.is_none() || self.reason(memory).is_some()
    }

    fn project_match(&self, in_scope: &[&Memory]) -> Option<ProjectMatch> {
        let mut project = self.project.clone()?;
        let mut by: BTreeSet<&'static str> = project.matched_by.iter().copied().collect();
        by.extend(in_scope.iter().filter_map(|m| self.reason(m)));
        project.matched_by = by.into_iter().collect();
        Some(project)
    }
}

/// The repository segment of a normalised remote (`host/owner/repo`).
fn repo_of(remote: &str) -> Option<&str> {
    remote.rsplit('/').next().filter(|r| !r.is_empty())
}

/// Resolve `name` to a scope, or `None` when no entity and no memory knows it.
fn resolve_scope(store: &Store<'_>, name: &str, memories: &[Memory]) -> Result<Option<Scope>> {
    let wanted = normalize_entity_name(name);
    let entity = resolve_entity(store, name)?;
    let mut names = HashSet::from([wanted.clone()]);
    let mut linked = HashSet::new();
    let mut matched_by = Vec::new();
    let project = match &entity {
        Some(entity) => {
            names.insert(normalize_entity_name(&entity.name));
            names.extend(entity.aliases.iter().map(|a| normalize_entity_name(a)));
            linked.extend(
                Entities::new(store)
                    .linked_memories(&entity.id, usize::MAX)?
                    .into_iter()
                    .map(|m| m.id),
            );
            if normalize_entity_name(&entity.name) != wanted {
                matched_by.push("alias");
            }
            ProjectMatch {
                name: entity.name.clone(),
                entity_id: Some(entity.id.clone()),
                aliases: entity.aliases.clone(),
                aliases_omitted: 0,
                matched_by,
            }
        }
        None => ProjectMatch {
            name: name.to_string(),
            entity_id: None,
            aliases: Vec::new(),
            aliases_omitted: 0,
            matched_by,
        },
    };
    let scope = Scope {
        names: Some(names),
        linked,
        project: Some(project),
    };
    let known = entity.is_some() || memories.iter().any(|m| scope.reason(m).is_some());
    Ok(known.then_some(scope))
}

/// Up to [`SUGGESTION_LIMIT`] known names close to `name`: containing it or
/// contained in it first, then by edit distance.
fn suggestions(store: &Store<'_>, name: &str, memories: &[Memory]) -> Result<Vec<String>> {
    let wanted = normalize_entity_name(name);
    let mut known: BTreeSet<String> = memories
        .iter()
        .flat_map(|m| {
            [
                m.project.as_deref(),
                m.git_remote.as_deref().and_then(repo_of),
            ]
        })
        .flatten()
        .map(str::to_string)
        .collect();
    known.extend(Entities::new(store).all()?.into_iter().map(|e| e.name));

    let mut scored: Vec<(usize, String)> = known
        .into_iter()
        .filter_map(|candidate| {
            let norm = normalize_entity_name(&candidate);
            let score = if norm.contains(&wanted) || wanted.contains(&norm) {
                0
            } else {
                let d = edit_distance(&norm, &wanted);
                if d > (wanted.chars().count() / 3).max(2) {
                    return None;
                }
                d
            };
            Some((score, candidate))
        })
        .collect();
    scored.sort();
    scored.dedup_by(|a, b| normalize_entity_name(&a.1) == normalize_entity_name(&b.1));
    Ok(scored
        .into_iter()
        .take(SUGGESTION_LIMIT)
        .map(|(_, name)| name)
        .collect())
}

/// Levenshtein distance over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != *cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

// -- sections -----------------------------------------------------------------

/// A section before the budget: its items in order, and its share weight.
struct Draft {
    name: &'static str,
    weight: usize,
    items: Vec<Item>,
}

fn first_line(content: &str) -> String {
    let line = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut text: String = line.chars().take(TEXT_CHARS).collect();
    if line.chars().count() > TEXT_CHARS {
        text.push('…');
    }
    text
}

fn kind_of(memory: &Memory) -> &str {
    memory
        .memory_type
        .as_deref()
        .filter(|t| *t != "unclassified")
        .unwrap_or(&memory.category)
}

fn item(memory: &Memory) -> Item {
    Item {
        id: memory.id.clone(),
        kind: kind_of(memory).to_string(),
        text: first_line(&memory.content),
        at: memory.created_at.clone(),
    }
}

fn after(timestamp: &str, start: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(timestamp).is_ok_and(|t| t >= start)
}

/// Not superseded, not expired, not sensitive, not a raw dialog: what is fit
/// to show unasked. Expiry compares parsed instants, never text: as text,
/// `13:00+02:00` (11:00Z) sorts after `12:00Z`. Shared with
/// [`crate::review::due_among`].
pub(crate) fn injectable(memory: &Memory, now: DateTime<Utc>) -> bool {
    memory.superseded_by.is_none()
        && !memory.sensitive
        && memory.category != crate::models::DIALOG_CATEGORY
        && memory.valid_until.as_deref().is_none_or(|until| {
            DateTime::parse_from_rfc3339(until).map_or(true, |until| until > now)
        })
}

/// Where an open memory sorts in the open section, or `None` if it is not
/// open: blockers in the window, then decisions, then action items.
fn open_rank(memory: &Memory, since: DateTime<Utc>) -> Option<u8> {
    let unresolved = memory.outcome.is_none();
    match memory.memory_type.as_deref() {
        Some("blocker") if after(&memory.created_at, since) => Some(0),
        Some("decision") if unresolved => Some(1),
        Some("action_item")
            if unresolved
                && !matches!(
                    memory.metadata.get("status").and_then(|s| s.as_str()),
                    Some("done" | "dropped")
                ) =>
        {
            Some(2)
        }
        _ => None,
    }
}

fn newest_first(a: &&Memory, b: &&Memory) -> std::cmp::Ordering {
    b.created_at.cmp(&a.created_at)
}

fn project_drafts(memories: &[&Memory], since: DateTime<Utc>, branch: Option<&str>) -> [Draft; 3] {
    let mut state: Vec<&Memory> = memories
        .iter()
        .copied()
        .filter(|m| m.category == "summary")
        .collect();
    state.sort_by(newest_first);
    state.truncate(STATE_LIMIT);

    let mut open: Vec<(u8, &Memory)> = memories
        .iter()
        .filter_map(|m| open_rank(m, since).map(|rank| (rank, *m)))
        .collect();
    open.sort_by(|(ra, a), (rb, b)| ra.cmp(rb).then_with(|| newest_first(a, b)));

    let shown: HashSet<&str> = state.iter().map(|m| m.id.as_str()).collect();
    let on_branch = |m: &Memory| branch.is_some() && m.git_branch.as_deref() == branch;
    let mut activity: Vec<&Memory> = memories
        .iter()
        .copied()
        .filter(|m| {
            after(&m.created_at, since)
                && open_rank(m, since).is_none()
                && m.memory_type.as_deref() != Some("action_item")
                && !shown.contains(m.id.as_str())
        })
        .collect();
    activity.sort_by(|a, b| {
        on_branch(b)
            .cmp(&on_branch(a))
            .then_with(|| newest_first(a, b))
    });

    [
        Draft {
            name: "state",
            weight: 20,
            items: state.into_iter().map(item).collect(),
        },
        Draft {
            name: "open",
            weight: 40,
            items: open
                .into_iter()
                .take(OPEN_LIMIT)
                .map(|(_, m)| item(m))
                .collect(),
        },
        Draft {
            name: "activity",
            weight: 40,
            items: activity
                .into_iter()
                .take(ACTIVITY_LIMIT)
                .map(item)
                .collect(),
        },
    ]
}

fn persona_draft(store: &Store<'_>) -> Result<Draft> {
    let items = crate::promotion::persona(store)?
        .into_iter()
        .take(PERSONA_LIMIT)
        .map(|s| Item {
            text: first_line(&s.content),
            id: s.id,
            kind: crate::models::PERSONA_CATEGORY.to_string(),
            at: s.created_at,
        })
        .collect();
    Ok(Draft {
        name: "persona",
        weight: 15,
        items,
    })
}

fn reminders_draft(store: &Store<'_>, now: DateTime<Utc>) -> Result<Draft> {
    let horizon = now + Duration::days(REMINDER_DAYS);
    let items = crate::reminders::list_reminders(store, ReminderWindow::All, 50)?
        .iter()
        .filter_map(|m| {
            let due = m.remind_at.as_deref()?;
            let in_horizon = DateTime::parse_from_rfc3339(due).is_ok_and(|t| t <= horizon);
            in_horizon.then(|| Item {
                at: due.to_string(),
                ..item(m)
            })
        })
        .collect();
    Ok(Draft {
        name: "reminders",
        weight: 10,
        items,
    })
}

/// Decisions in scope due for a follow-up review (`crate::review`), oldest
/// due date first. Listed here, they are left out of the project sections.
fn reviews_draft(memories: &[Memory], scope: &Scope, now: DateTime<Utc>) -> Draft {
    let items = crate::review::due_among(memories, now, None)
        .into_iter()
        .filter(|m| scope.admits(m))
        .take(REVIEW_LIMIT)
        .map(|m| Item {
            at: crate::review::review_due(m).unwrap_or_default().to_string(),
            ..item(m)
        })
        .collect();
    Draft {
        name: "reviews",
        weight: 15,
        items,
    }
}

fn hits_draft(store: &Store<'_>, prompt: &str, now: DateTime<Utc>) -> Result<Draft> {
    let input = MemorySearchInput {
        query: prompt.to_string(),
        limit: HIT_LIMIT * 2,
        ..MemorySearchInput::default()
    };
    let items = crate::db::queries::search_memories(store, &input)?
        .iter()
        .map(|r| &r.memory)
        .filter(|m| injectable(m, now))
        .take(HIT_LIMIT)
        .map(item)
        .collect();
    Ok(Draft {
        name: "hits",
        weight: 25,
        items,
    })
}

// -- budget -------------------------------------------------------------------

/// Estimated tokens of `bytes` bytes, rounded up, so the sum over pieces
/// bounds the estimate of the whole.
fn tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

fn json_len(value: &impl Serialize) -> usize {
    serde_json::to_string(value).map_or(0, |s| s.len())
}

/// An item: its JSON plus a separator, or its Markdown line.
fn item_cost(section: &str, item: &Item) -> usize {
    tokens((json_len(item) + 1).max(line(section, item).len() + 1))
}

/// A section without items, with `omitted` items noted.
fn section_cost(name: &'static str, omitted: usize) -> usize {
    let skeleton = Section {
        name,
        items: Vec::new(),
        omitted,
    };
    tokens((json_len(&skeleton) + 1).max(section_markdown(&skeleton).len()))
}

/// The brief without its sections: the JSON envelope or the Markdown title.
fn envelope_cost(brief: &Brief) -> usize {
    let sized = Brief {
        sections: Vec::new(),
        budget: Budget {
            limit: brief.budget.limit,
            used: brief.budget.limit,
        },
        ..brief.clone()
    };
    tokens(json_len(&sized).max(title(brief).len()))
}

/// The envelope and every section's heading.
fn overhead(brief: &Brief, drafts: &[Draft]) -> usize {
    envelope_cost(brief)
        + drafts
            .iter()
            .map(|d| section_cost(d.name, d.items.len()))
            .sum::<usize>()
}

/// Cut aliases from the end until the overhead fits `limit`, or refuse.
fn fit_overhead(brief: &mut Brief, drafts: &[Draft]) -> Result<usize> {
    let limit = brief.budget.limit;
    loop {
        let cost = overhead(brief, drafts);
        if limit == 0 || cost <= limit {
            return Ok(cost);
        }
        let Some(project) = brief.project.as_mut().filter(|p| !p.aliases.is_empty()) else {
            return Err(StoreError::Invalid(format!(
                "`token_budget` {limit} is too small for this brief; it needs at least {cost}"
            )));
        };
        project.aliases.pop();
        project.aliases_omitted += 1;
    }
}

/// Reserve the envelope and every section's heading, then allot what is
/// left across `drafts` in order, each a share of the remainder by its
/// weight, and keep the items that fit.
fn fit(brief: &mut Brief, drafts: Vec<Draft>) -> Result<()> {
    let limit = brief.budget.limit;
    let overhead = fit_overhead(brief, &drafts)?;
    let available = limit.saturating_sub(overhead);
    let mut weight_left: usize = drafts.iter().map(|d| d.weight).sum();
    let mut spent_total = 0;
    for draft in drafts {
        let allotted = if limit == 0 {
            usize::MAX
        } else {
            (available - spent_total) * draft.weight / weight_left.max(1)
        };
        weight_left -= draft.weight;
        let total = draft.items.len();
        let mut spent = 0;
        let mut items = Vec::new();
        for item in draft.items {
            let c = item_cost(draft.name, &item);
            if spent + c > allotted {
                break;
            }
            spent += c;
            items.push(item);
        }
        spent_total += spent;
        brief.sections.push(Section {
            name: draft.name,
            omitted: total - items.len(),
            items,
        });
    }
    brief.budget.used = overhead + spent_total;
    Ok(())
}

// -- rendering ----------------------------------------------------------------

fn heading(name: &str) -> &'static str {
    match name {
        "persona" => "Persona",
        "reminders" => "Reminders due within 7 days",
        "reviews" => "Decisions to review: did they hold up?",
        "state" => "Current state",
        "open" => "Open: blockers, decisions, action items",
        "activity" => "Recent activity",
        _ => "Related to this prompt",
    }
}

fn line(section: &str, item: &Item) -> String {
    match section {
        "persona" => format!("- {}", item.text),
        "reminders" | "reviews" => format!("- due {}: {} ({})", item.at, item.text, item.id),
        _ => format!("- [{}] {} ({})", item.kind, item.text, item.id),
    }
}

/// Render as Markdown.
///
/// A [`Mode::Project`] brief shows every section, an empty one as "None", so
/// a reader can tell "nothing open" from "not looked at". The hooks' briefs
/// leave empty sections out, and are empty when every section is.
pub fn render_markdown(brief: &Brief) -> String {
    let show_empty = brief.mode == Mode::Project;
    let shown: Vec<&Section> = brief
        .sections
        .iter()
        .filter(|s| show_empty || !s.items.is_empty())
        .collect();
    if shown.is_empty() {
        return String::new();
    }
    let mut out = title(brief);
    for section in shown {
        out.push_str(&section_markdown(section));
    }
    out
}

fn section_markdown(section: &Section) -> String {
    let mut out = format!("\n## {}\n", heading(section.name));
    if section.items.is_empty() {
        out.push_str("_None._\n");
    }
    for item in &section.items {
        out.push_str(&line(section.name, item));
        out.push('\n');
    }
    if section.omitted > 0 {
        out.push_str(&format!(
            "_{} more not shown; raise the budget._\n",
            section.omitted
        ));
    }
    out
}

fn title(brief: &Brief) -> String {
    match brief.mode {
        Mode::Prompt => "# Possibly relevant memories\n".to_string(),
        Mode::Project => {
            let name = brief.project.as_ref().map_or("", |p| p.name.as_str());
            format!("# Brief: {name}\n_Activity since {}._\n", brief.since)
        }
        Mode::Session => {
            let scope = [
                brief
                    .project
                    .as_ref()
                    .map(|p| format!("project: {}", p.name)),
                brief.branch.as_ref().map(|b| format!("branch: {b}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            if scope.is_empty() {
                "# Memory context\n".to_string()
            } else {
                format!("# Memory context ({scope})\n")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;
    use crate::models::EntityInput;
    use crate::retrieval::estimated_content_tokens;

    #[test]
    fn expiry_compares_instants_not_text() {
        let now: DateTime<Utc> = "2026-10-26T12:00:00Z".parse().unwrap();
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let cases = [
            ("mem_a", "2026-10-26T13:00:00+02:00", false),
            ("mem_b", "2026-10-26T15:00:00+02:00", true),
            ("mem_c", "2026-10-26T12:00:00Z", false),
        ];
        for (id, until, _) in cases {
            add(&store, id, |r| r.valid_until = Some(until.into()));
        }
        for (id, until, shown) in cases {
            let memory = Memories::new(&store).get_live(id).unwrap().unwrap();
            assert_eq!(injectable(&memory, now), shown, "{until}");
        }
    }

    fn add(store: &Store<'_>, id: &str, f: impl FnOnce(&mut NewMemory)) {
        let mut row = NewMemory::new(id, format!("content of {id}"), &Utc::now().to_rfc3339());
        row.category = "fact".into();
        f(&mut row);
        Memories::new(store).insert(&row).unwrap();
    }

    fn day(d: i64) -> String {
        (Utc::now() - Duration::days(d)).to_rfc3339()
    }

    fn seed(store: &Store<'_>) {
        add(store, "mem_old_main", |r| {
            r.project = Some("quokka".into());
            r.git_branch = Some("main".into());
            r.created_at = day(1);
        });
        add(store, "mem_new_main", |r| {
            r.project = Some("quokka".into());
            r.git_branch = Some("main".into());
            r.created_at = day(0);
        });
        add(store, "mem_feature", |r| {
            r.project = Some("quokka".into());
            r.git_branch = Some("feat".into());
            r.created_at = day(5);
        });
        add(store, "mem_other_project", |r| {
            r.project = Some("elsewhere".into())
        });
        add(store, "mem_secret", |r| {
            r.project = Some("quokka".into());
            r.sensitive = true;
        });
        add(store, "mem_expired", |r| {
            r.project = Some("quokka".into());
            r.valid_until = Some(day(2));
        });
        add(store, "mem_todo", |r| {
            r.project = Some("quokka".into());
            r.category = "action_item".into();
            r.memory_type = "action_item".into();
        });
        add(store, "mem_done", |r| {
            r.project = Some("quokka".into());
            r.memory_type = "action_item".into();
            r.metadata = serde_json::json!({"status": "done"});
        });
        add(store, "mem_closed", |r| {
            r.project = Some("quokka".into());
            r.memory_type = "action_item".into();
            r.outcome = Some("done".into());
        });
    }

    fn session(budget: usize) -> ContextRequest<'static> {
        ContextRequest {
            project: Some("quokka"),
            branch: Some("feat"),
            budget,
            ..ContextRequest::default()
        }
    }

    fn project(name: &'static str) -> ContextRequest<'static> {
        ContextRequest {
            mode: Mode::Project,
            project: Some(name),
            budget: DEFAULT_TOKEN_BUDGET,
            ..ContextRequest::default()
        }
    }

    fn brief(store: &Store<'_>, request: &ContextRequest<'_>) -> Brief {
        match build(store, request).unwrap() {
            Outcome::Brief(brief) => brief,
            Outcome::UnknownProject { name, .. } => panic!("{name} unknown"),
        }
    }

    fn ids(brief: &Brief, section: &str) -> Vec<String> {
        brief
            .sections
            .iter()
            .find(|s| s.name == section)
            .map(|s| s.items.iter().map(|i| i.id.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn scopes_to_project_branch_first_and_hides_unfit_memories() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        let brief = brief(&store, &session(0));
        let text = render_markdown(&brief);

        assert_eq!(ids(&brief, "open"), ["mem_todo"], "{text}");
        for hidden in [
            "mem_done",
            "mem_closed",
            "mem_other_project",
            "mem_secret",
            "mem_expired",
        ] {
            assert!(!text.contains(hidden), "{hidden} shown: {text}");
        }
        assert_eq!(
            ids(&brief, "activity"),
            ["mem_feature", "mem_new_main", "mem_old_main"],
            "the branch's memory leads, then newest first"
        );
        assert!(text.starts_with("# Memory context (project: quokka, branch: feat)"));
        assert_eq!(brief.budget.limit, 0);
    }

    #[test]
    fn nothing_to_say_is_an_empty_context() {
        let db = Database::open_in_memory().unwrap();
        let brief = brief(&db.store(), &session(500));
        assert!(brief.sections.iter().all(|s| s.items.is_empty()));
        assert_eq!(render_markdown(&brief), "");
    }

    #[test]
    fn a_prompt_adds_hits_and_due_reminders_are_listed() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        add(&store, "mem_flaky", |r| {
            r.content = "the flaky scheduler test is a race".into();
        });
        add(&store, "mem_remind", |r| {
            r.content = "renew the certificate".into();
            r.remind_at = Some((Utc::now() + Duration::days(2)).to_rfc3339());
        });
        add(&store, "mem_far", |r| {
            r.content = "much later".into();
            r.remind_at = Some((Utc::now() + Duration::days(30)).to_rfc3339());
        });
        let req = ContextRequest {
            prompt: Some("flaky scheduler"),
            ..ContextRequest::default()
        };
        let brief = brief(&store, &req);
        assert_eq!(ids(&brief, "reminders"), ["mem_remind"]);
        assert!(ids(&brief, "hits").contains(&"mem_flaky".to_string()));
        assert!(render_markdown(&brief).contains("Related to this prompt"));
    }

    #[test]
    fn prompt_mode_returns_just_the_prompt_matches_and_nothing_without_one() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        add(&store, "mem_flaky", |r| {
            r.content = "the flaky scheduler test is a race".into();
            r.project = Some("quokka".into());
        });
        let req = ContextRequest {
            mode: Mode::Prompt,
            project: Some("quokka"),
            prompt: Some("flaky scheduler"),
            ..ContextRequest::default()
        };
        let hits = brief(&store, &req);
        assert!(render_markdown(&hits).starts_with("# Possibly relevant memories"));
        assert_eq!(
            hits.sections.iter().map(|s| s.name).collect::<Vec<_>>(),
            ["hits"]
        );
        assert!(!hits.sections[0].items.is_empty());

        let none = brief(
            &store,
            &ContextRequest {
                mode: Mode::Prompt,
                ..ContextRequest::default()
            },
        );
        assert!(none.sections.is_empty());
        assert_eq!(render_markdown(&none), "");
    }

    #[test]
    fn project_mode_has_state_open_and_windowed_activity() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        add(&store, "mem_summary_old", |r| {
            r.project = Some("quokka".into());
            r.category = "summary".into();
            r.created_at = day(40);
        });
        add(&store, "mem_decision", |r| {
            r.project = Some("quokka".into());
            r.memory_type = "decision".into();
            r.created_at = day(30);
        });
        add(&store, "mem_blocker_new", |r| {
            r.project = Some("quokka".into());
            r.memory_type = "blocker".into();
        });
        add(&store, "mem_blocker_old", |r| {
            r.project = Some("quokka".into());
            r.memory_type = "blocker".into();
            r.created_at = day(30);
        });
        let req = ContextRequest {
            since: Some("2d"),
            ..project("quokka")
        };
        let brief = brief(&store, &req);

        assert_eq!(
            ids(&brief, "state"),
            ["mem_summary_old"],
            "state is not windowed"
        );
        assert_eq!(
            ids(&brief, "open"),
            ["mem_blocker_new", "mem_decision", "mem_todo"],
            "blockers in the window, then decisions, then action items"
        );
        assert_eq!(ids(&brief, "activity"), ["mem_new_main", "mem_old_main"]);
        assert_eq!(
            brief.sections.iter().map(|s| s.name).collect::<Vec<_>>(),
            ["state", "open", "activity"],
            "no persona or reminders"
        );
        assert_eq!(brief.project.as_ref().unwrap().matched_by, ["project"]);
    }

    #[test]
    fn an_empty_window_still_shows_what_is_open() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        add(&store, "mem_todo", |r| {
            r.project = Some("quokka".into());
            r.memory_type = "action_item".into();
            r.created_at = day(60);
        });
        add(&store, "mem_old", |r| {
            r.project = Some("quokka".into());
            r.created_at = day(60);
        });
        let brief = brief(&store, &project("quokka"));
        assert_eq!(ids(&brief, "open"), ["mem_todo"]);
        assert!(ids(&brief, "activity").is_empty());
        let text = render_markdown(&brief);
        assert!(text.contains("## Recent activity\n_None._"), "{text}");
    }

    #[test]
    fn an_alias_resolves_through_the_entity_and_its_links() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        crate::entity::upsert_entity(
            &store,
            &EntityInput {
                name: "rusty_mux".into(),
                kind: Some("project".into()),
                aliases: vec!["mux".into()],
            },
        )
        .unwrap();
        add(&store, "mem_by_project", |r| {
            r.project = Some("Rusty_Mux".into())
        });
        add(&store, "mem_by_remote", |r| {
            r.git_remote = Some("github.com/rusty-mill/rusty_mux".into())
        });
        add(&store, "mem_by_tag", |r| r.tags = vec!["mux".into()]);
        add(&store, "mem_linked", |r| {
            r.project = Some("rusty_mill".into())
        });
        add(&store, "mem_unrelated", |r| {
            r.project = Some("rusty_term".into())
        });
        let linked = crate::entity::entity_id("rusty_mux");
        crate::entity::link_memory_entity(&store, "mem_linked", &linked).unwrap();

        let brief = brief(&store, &project("mux"));
        let mut found = ids(&brief, "activity");
        found.sort();
        assert_eq!(
            found,
            [
                "mem_by_project",
                "mem_by_remote",
                "mem_by_tag",
                "mem_linked"
            ]
        );
        let matched = brief.project.unwrap();
        assert_eq!(matched.name, "rusty_mux");
        assert_eq!(
            matched.matched_by,
            ["alias", "entity", "git_remote", "project", "tag"]
        );
    }

    #[test]
    fn an_unknown_project_is_refused_with_suggestions_not_guessed() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        add(&store, "mem_mux", |r| r.project = Some("rusty_mux".into()));
        match build(&store, &project("quoka")).unwrap() {
            Outcome::UnknownProject { name, suggestions } => {
                assert_eq!(name, "quoka");
                assert_eq!(suggestions, ["quokka"]);
            }
            Outcome::Brief(b) => panic!("guessed {:?}", b.project),
        }
        match build(&store, &project("mux")).unwrap() {
            Outcome::UnknownProject { suggestions, .. } => assert_eq!(suggestions, ["rusty_mux"]),
            Outcome::Brief(b) => panic!("guessed {:?}", b.project),
        }
        // The session hook names whatever directory it is in: no project
        // sections, no refusal.
        let hook = brief(
            &store,
            &ContextRequest {
                project: Some("nowhere"),
                ..ContextRequest::default()
            },
        );
        assert!(hook.project.is_none());
        assert!(ids(&hook, "activity").is_empty());
    }

    #[test]
    fn budget_overflow_drops_whole_items_counts_them_and_rolls_forward() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        for i in 0..30 {
            add(&store, &format!("mem_{i:02}"), |r| {
                r.project = Some("quokka".into());
                r.content = format!("{i} {}", "word ".repeat(40));
            });
        }
        let req = ContextRequest {
            budget: MIN_TOKEN_BUDGET,
            ..project("quokka")
        };
        let tight = brief(&store, &req);
        let activity = tight
            .sections
            .iter()
            .find(|s| s.name == "activity")
            .unwrap();
        assert!(
            !activity.items.is_empty(),
            "state and open roll their share forward"
        );
        assert_eq!(activity.items.len() + activity.omitted, ACTIVITY_LIMIT);
        assert!(activity.omitted > 0);
        assert!(tight.budget.used <= MIN_TOKEN_BUDGET, "{:?}", tight.budget);
        assert!(activity
            .items
            .iter()
            .all(|i| i.text.chars().count() <= TEXT_CHARS + 1));
        assert!(render_markdown(&tight).contains("more not shown"));
    }

    #[test]
    fn either_rendering_stays_within_the_budget() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        for i in 0..30 {
            add(&store, &format!("mem_{i:02}"), |r| {
                r.project = Some("quokka".into());
                r.memory_type = if i % 3 == 0 { "decision" } else { "fact" }.into();
                r.content = format!("{i} {}", "word ".repeat(10 + i * 3));
            });
        }
        for limit in [
            MIN_TOKEN_BUDGET,
            500,
            DEFAULT_TOKEN_BUDGET,
            MAX_TOKEN_BUDGET,
        ] {
            let req = ContextRequest {
                budget: limit,
                ..project("quokka")
            };
            let brief = brief(&store, &req);
            let json = estimated_content_tokens(&serde_json::to_string(&brief).unwrap());
            let markdown = estimated_content_tokens(&render_markdown(&brief));
            assert!(brief.budget.used <= limit, "{limit}: {:?}", brief.budget);
            assert!(
                json <= brief.budget.used,
                "{limit}: json {json} > {:?}",
                brief.budget
            );
            assert!(
                markdown <= brief.budget.used,
                "{limit}: md {markdown} > {:?}",
                brief.budget
            );
        }
    }

    #[test]
    fn long_aliases_are_cut_to_fit_and_an_impossible_budget_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let aliases: Vec<String> = (0..300).map(|i| format!("quokka-alias-{i:03}")).collect();
        crate::entity::upsert_entity(
            &store,
            &EntityInput {
                name: "quokka".into(),
                kind: Some("project".into()),
                aliases,
            },
        )
        .unwrap();
        add(&store, "mem_q", |r| r.project = Some("quokka".into()));
        let tight = brief(
            &store,
            &ContextRequest {
                budget: MIN_TOKEN_BUDGET,
                ..project("quokka")
            },
        );
        let matched = tight.project.as_ref().unwrap();
        assert!(matched.aliases_omitted > 0);
        assert_eq!(matched.aliases.len() + matched.aliases_omitted, 300);
        assert!(tight.budget.used <= MIN_TOKEN_BUDGET);
        let json = estimated_content_tokens(&serde_json::to_string(&tight).unwrap());
        assert!(json <= MIN_TOKEN_BUDGET, "{json}");

        let long_name = "q".repeat(2000);
        add(&store, "mem_long", |r| r.project = Some(long_name.clone()));
        let req = ContextRequest {
            mode: Mode::Project,
            project: Some(&long_name),
            budget: MIN_TOKEN_BUDGET,
            ..ContextRequest::default()
        };
        let refused = build(&store, &req).unwrap_err();
        assert!(refused.to_string().contains("too small"), "{refused}");
    }

    #[test]
    fn hidden_rows_neither_establish_nor_suggest_a_project() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        add(&store, "mem_secret", |r| {
            r.project = Some("hush_project".into());
            r.sensitive = true;
        });
        add(&store, "mem_gone", |r| {
            r.git_remote = Some("github.com/o/old_repo".into());
            r.superseded_by = Some("mem_secret".into());
        });
        for name in ["hush_project", "hush_projec", "old_repo"] {
            match build(&store, &project(name)).unwrap() {
                Outcome::UnknownProject { suggestions, .. } => {
                    assert!(suggestions.is_empty(), "{name}: {suggestions:?}")
                }
                Outcome::Brief(b) => panic!("{name} known through a hidden row: {:?}", b.project),
            }
        }
    }

    #[test]
    fn a_project_brief_requires_a_name_but_a_session_does_not() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        for blank in [None, Some(""), Some("   ")] {
            let req = ContextRequest {
                mode: Mode::Project,
                project: blank,
                budget: DEFAULT_TOKEN_BUDGET,
                ..ContextRequest::default()
            };
            assert!(build(&store, &req).is_err(), "{blank:?} accepted");
        }
        let unscoped = brief(&store, &ContextRequest::default());
        assert!(ids(&unscoped, "activity").contains(&"mem_other_project".to_string()));
    }

    #[test]
    fn since_accepts_days_hours_and_rfc3339_and_refuses_the_rest() {
        let now = Utc::now();
        assert_eq!(parse_since("7d", now).unwrap(), now - Duration::days(7));
        assert_eq!(parse_since("12h", now).unwrap(), now - Duration::hours(12));
        let at = (now - Duration::days(3)).to_rfc3339();
        assert_eq!(parse_since(&at, now).unwrap().to_rfc3339(), at);
        for bad in [
            "",
            "0d",
            "-1d",
            "7",
            "7w",
            "366d",
            "99999999999999d",
            "soon",
        ] {
            assert!(parse_since(bad, now).is_err(), "{bad:?} accepted");
        }
        let future = (now + Duration::days(1)).to_rfc3339();
        assert!(parse_since(&future, now).is_err());
    }

    #[test]
    fn since_takes_a_date_phrase_s_start_within_the_cap() {
        let now = DateTime::parse_from_rfc3339("2026-10-28T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let utc = || Ok(chrono_tz::Tz::UTC);
        let start = |raw: &str| parse_since_with(raw, now, utc).map(|t| t.to_rfc3339());
        assert_eq!(start("yesterday").unwrap(), "2026-10-27T00:00:00+00:00");
        assert_eq!(start("last week").unwrap(), "2026-10-19T00:00:00+00:00");
        assert_eq!(start("since monday").unwrap(), "2026-10-26T00:00:00+00:00");
        assert_eq!(start("this year").unwrap(), "2026-01-01T00:00:00+00:00");
        assert!(
            start("last year").is_err(),
            "more than MAX_WINDOW_DAYS back"
        );
        assert_eq!(start("in november").unwrap(), "2025-11-01T00:00:00+00:00");
        assert!(start("2026-11").is_err(), "starts in the future");
        assert!(start("past 5 minutes").is_err(), "not a phrase");
    }

    #[test]
    fn token_budget_bounds_are_inclusive() {
        assert!(check_token_budget(MIN_TOKEN_BUDGET).is_ok());
        assert!(check_token_budget(MAX_TOKEN_BUDGET).is_ok());
        assert!(check_token_budget(MIN_TOKEN_BUDGET - 1).is_err());
        assert!(check_token_budget(MAX_TOKEN_BUDGET + 1).is_err());
    }

    #[test]
    fn edit_distance_counts_single_char_edits() {
        assert_eq!(edit_distance("quokka", "quoka"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }
}
