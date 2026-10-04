//! The brief a session starts with: what is worth knowing before the first
//! prompt, within a character budget.
//!
//! Five sections, in this order, each omitted when empty:
//! 1. persona (the durable statements the refinement ladder promoted),
//! 2. reminders due within a week (overdue ones included),
//! 3. open action items for the project,
//! 4. the project's most recent memories, the current branch's first,
//! 5. the best matches for the user's prompt, when one is given.
//!
//! Trimming happens at section boundaries: a section that does not fit the
//! remaining budget is dropped whole and named in `dropped`, never cut
//! mid-line, so what the model reads is always complete statements. Earlier
//! sections have priority because they are the more durable.

use crate::db::memories::Memories;
use crate::db::{Result, Store};
use crate::models::{Memory, MemorySearchInput, ReminderWindow};
use chrono::{Duration, Utc};
use serde::Serialize;

/// Recent memories listed.
pub const RECENT_LIMIT: usize = 8;
/// Open action items listed.
pub const ACTION_LIMIT: usize = 8;
/// Prompt hits listed.
pub const HIT_LIMIT: usize = 5;
/// Persona statements listed.
pub const PERSONA_LIMIT: usize = 10;
/// Days ahead a reminder counts as due.
pub const REMINDER_DAYS: i64 = 7;
/// A memory line is cut to this many characters.
const LINE_CHARS: usize = 240;

/// What to build a brief for.
#[derive(Debug, Clone, Default)]
pub struct ContextRequest<'a> {
    pub project: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub prompt: Option<&'a str>,
    /// Characters; `0` means unlimited.
    pub budget: usize,
}

/// How many items each section held (0 when it was empty or dropped).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SectionCounts {
    pub persona: usize,
    pub reminders: usize,
    pub action_items: usize,
    pub recent: usize,
    pub hits: usize,
}

/// The rendered brief.
#[derive(Debug, Clone, Serialize)]
pub struct ContextBrief {
    pub context: String,
    pub sections: SectionCounts,
    /// Sections that did not fit the budget.
    pub dropped: Vec<&'static str>,
}

struct Section {
    name: &'static str,
    heading: &'static str,
    lines: Vec<String>,
}

impl Section {
    fn render(&self) -> String {
        format!("## {}\n{}\n", self.heading, self.lines.join("\n"))
    }
}

fn line_of(memory: &Memory) -> String {
    let first = memory.content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let text: String = first.trim().chars().take(LINE_CHARS).collect();
    format!("- [{}] {} ({})", memory.category, text, memory.id)
}

/// Not superseded, not expired, not sensitive, not a raw dialog: what is fit
/// to inject unasked.
fn injectable(memory: &Memory, now: &str) -> bool {
    memory.superseded_by.is_none()
        && !memory.sensitive
        && memory.category != crate::models::DIALOG_CATEGORY
        && memory.valid_until.as_deref().is_none_or(|until| until > now)
}

/// Filter on `project` in Rust over the memories.
// TODO(wave1B): replace with the store's `project` filter once it lands.
fn in_project(memory: &Memory, project: Option<&str>) -> bool {
    project.is_none_or(|p| memory.project.as_deref() == Some(p))
}

fn persona_section(store: &Store<'_>) -> Result<Section> {
    let lines = crate::promotion::persona(store)?
        .into_iter()
        .take(PERSONA_LIMIT)
        .map(|s| format!("- {}", s.content.lines().next().unwrap_or("").trim()))
        .collect();
    Ok(Section {
        name: "persona",
        heading: "Persona",
        lines,
    })
}

fn reminders_section(store: &Store<'_>) -> Result<Section> {
    let horizon = (Utc::now() + Duration::days(REMINDER_DAYS)).to_rfc3339();
    let lines = crate::reminders::list_reminders(store, ReminderWindow::All, 50)?
        .iter()
        .filter(|m| m.remind_at.as_deref().is_some_and(|at| at <= horizon.as_str()))
        .map(|m| format!("- {}: {}", m.remind_at.as_deref().unwrap_or(""), line_of(m)))
        .collect();
    Ok(Section {
        name: "reminders",
        heading: "Reminders due within 7 days",
        lines,
    })
}

fn is_open_action(memory: &Memory) -> bool {
    memory.memory_type.as_deref() == Some("action_item")
        && memory.outcome.is_none()
        && memory.metadata.get("status").and_then(|s| s.as_str()) != Some("done")
}

fn action_section(memories: &[Memory], project: Option<&str>, now: &str) -> Section {
    let mut open: Vec<&Memory> = memories
        .iter()
        .filter(|m| is_open_action(m) && injectable(m, now) && in_project(m, project))
        .collect();
    open.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Section {
        name: "action_items",
        heading: "Open action items",
        lines: open.into_iter().take(ACTION_LIMIT).map(line_of).collect(),
    }
}

fn recent_section(
    memories: &[Memory],
    project: Option<&str>,
    branch: Option<&str>,
    now: &str,
) -> Section {
    let mut recent: Vec<&Memory> = memories
        .iter()
        .filter(|m| injectable(m, now) && in_project(m, project) && !is_open_action(m))
        .collect();
    // The branch's own memories first, newest first within each group.
    let on_branch = |m: &Memory| branch.is_some() && m.git_branch.as_deref() == branch;
    recent.sort_by(|a, b| {
        on_branch(b)
            .cmp(&on_branch(a))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    Section {
        name: "recent",
        heading: "Recent memories",
        lines: recent.into_iter().take(RECENT_LIMIT).map(line_of).collect(),
    }
}

fn hits_section(store: &Store<'_>, prompt: &str, now: &str) -> Result<Section> {
    let input = MemorySearchInput {
        query: prompt.to_string(),
        limit: HIT_LIMIT * 2,
        ..MemorySearchInput::default()
    };
    let lines = crate::db::queries::search_memories(store, &input)?
        .iter()
        .map(|r| &r.memory)
        .filter(|m| injectable(m, now))
        .take(HIT_LIMIT)
        .map(line_of)
        .collect();
    Ok(Section {
        name: "hits",
        heading: "Related to this prompt",
        lines,
    })
}

/// Build the brief for `request`.
pub fn build(store: &Store<'_>, request: &ContextRequest<'_>) -> Result<ContextBrief> {
    let now = Utc::now().to_rfc3339();
    let memories = Memories::new(store).all_live()?;
    let mut sections = vec![
        persona_section(store)?,
        reminders_section(store)?,
        action_section(&memories, request.project, &now),
        recent_section(&memories, request.project, request.branch, &now),
    ];
    if let Some(prompt) = request.prompt.map(str::trim).filter(|p| !p.is_empty()) {
        sections.push(hits_section(store, prompt, &now)?);
    }
    Ok(assemble(sections, request))
}

fn header(request: &ContextRequest<'_>) -> String {
    let scope = [
        request.project.map(|p| format!("project: {p}")),
        request.branch.map(|b| format!("branch: {b}")),
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

/// Join the non-empty sections in order, dropping each that would overrun
/// the budget. A dropped section does not stop a later, smaller one.
fn assemble(sections: Vec<Section>, request: &ContextRequest<'_>) -> ContextBrief {
    let mut context = header(request);
    let mut counts = SectionCounts::default();
    let mut dropped = Vec::new();
    for section in sections.iter().filter(|s| !s.lines.is_empty()) {
        let text = section.render();
        let used = context.chars().count() + 1 + text.chars().count();
        if request.budget != 0 && used > request.budget {
            dropped.push(section.name);
            continue;
        }
        context.push('\n');
        context.push_str(&text);
        let n = section.lines.len();
        match section.name {
            "persona" => counts.persona = n,
            "reminders" => counts.reminders = n,
            "action_items" => counts.action_items = n,
            "recent" => counts.recent = n,
            _ => counts.hits = n,
        }
    }
    // Nothing fit, or nothing exists: an empty brief is empty, not a
    // header with no body.
    if counts == SectionCounts::default() {
        context.clear();
    }
    ContextBrief {
        context,
        sections: counts,
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;

    fn add(store: &Store<'_>, id: &str, f: impl FnOnce(&mut NewMemory)) {
        let mut row = NewMemory::new(id, format!("content of {id}"), &Utc::now().to_rfc3339());
        row.category = "fact".into();
        f(&mut row);
        Memories::new(store).insert(&row).unwrap();
    }

    fn seed(store: &Store<'_>) {
        let day = |d: i64| (Utc::now() - Duration::days(d)).to_rfc3339();
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
        add(store, "mem_other_project", |r| r.project = Some("elsewhere".into()));
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

    fn request(budget: usize) -> ContextRequest<'static> {
        ContextRequest {
            project: Some("quokka"),
            branch: Some("feat"),
            budget,
            ..ContextRequest::default()
        }
    }

    #[test]
    fn scopes_to_project_branch_first_and_hides_unfit_memories() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        let brief = build(&store, &request(0)).unwrap();

        assert_eq!(brief.sections.action_items, 1, "{}", brief.context);
        assert!(brief.context.contains("mem_todo"));
        assert!(!brief.context.contains("mem_done") && !brief.context.contains("mem_closed"));
        assert!(!brief.context.contains("mem_other_project"));
        assert!(!brief.context.contains("mem_secret"));
        assert!(!brief.context.contains("mem_expired"));
        let feature = brief.context.find("mem_feature").unwrap();
        let newest = brief.context.find("mem_new_main").unwrap();
        assert!(feature < newest, "the branch's memory leads: {}", brief.context);
        assert!(brief.context.starts_with("# Memory context (project: quokka, branch: feat)"));
        assert!(brief.dropped.is_empty());
    }

    #[test]
    fn a_tight_budget_drops_whole_sections_and_names_them() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        seed(&store);
        let full = build(&store, &request(0)).unwrap();
        // Room for the action-item section but not the recent list.
        let tight = build(&store, &request(210)).unwrap();
        assert!(tight.context.chars().count() <= 210);
        assert_eq!(tight.sections.action_items, 1);
        assert_eq!(tight.dropped, vec!["recent"]);
        assert!(full.context.chars().count() > 210);
    }

    #[test]
    fn nothing_to_say_is_an_empty_context() {
        let db = Database::open_in_memory().unwrap();
        let brief = build(&db.store(), &request(500)).unwrap();
        assert_eq!(brief.context, "");
        assert_eq!(brief.sections, SectionCounts::default());
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
        let brief = build(&store, &req).unwrap();
        assert_eq!(brief.sections.reminders, 1, "{}", brief.context);
        assert!(brief.context.contains("renew the certificate"));
        assert!(!brief.context.contains("much later"));
        assert!(brief.sections.hits >= 1, "{}", brief.context);
        assert!(brief.context.contains("Related to this prompt"));
    }
}
