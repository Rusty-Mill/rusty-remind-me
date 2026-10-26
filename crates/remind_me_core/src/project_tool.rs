//! The one surface over project records (ADR-0027 section 6).
//!
//! `remind_me_project` and `/api/projects` both call [`call`], so a dashboard,
//! Claude and any other client see the same data and the same validation. The
//! action set is closed, every error is a plain sentence, and the payloads are
//! ordinary JSON with no client-specific shape: a small local model reads them
//! as easily as a large one (#65).
//!
//! `list` is deliberately compact — a name, a status, the anchors and how many
//! memories back each claim — because a listing is read at the start of a
//! session. The evidence ids themselves come from `get` or `history`, for the
//! one project someone is asking about.

use crate::db::{Result, Store, StoreError};
use crate::project::{self, Claim, ClaimValue, Predicate, Project, Stage, Status, WriteOutcome};
use crate::project_backfill;
use crate::project_proposals;
use serde_json::{json, Map, Value};
use std::str::FromStr;

/// What a caller may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    List,
    Get,
    History,
    Proposals,
    Confirm,
    Reject,
    Set,
    Revert,
    Backfill,
}

impl Action {
    pub const ALL: [Action; 9] = [
        Self::List,
        Self::Get,
        Self::History,
        Self::Proposals,
        Self::Confirm,
        Self::Reject,
        Self::Set,
        Self::Revert,
        Self::Backfill,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Get => "get",
            Self::History => "history",
            Self::Proposals => "proposals",
            Self::Confirm => "confirm",
            Self::Reject => "reject",
            Self::Set => "set",
            Self::Revert => "revert",
            Self::Backfill => "backfill",
        }
    }

    /// Whether this action writes, which is what decides its tool tier.
    pub fn writes(self) -> bool {
        matches!(
            self,
            Self::Confirm | Self::Reject | Self::Set | Self::Revert | Self::Backfill
        )
    }
}

impl FromStr for Action {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == s.trim().to_ascii_lowercase())
            .ok_or_else(|| {
                StoreError::Invalid(format!(
                    "unknown action '{s}' (expected one of: {})",
                    Self::ALL.map(Action::as_str).join(", ")
                ))
            })
    }
}

/// Run one call. `args` is the tool's or route's arguments as given.
pub fn call(store: &Store<'_>, args: &Value) -> Result<Value> {
    let action = match text(args, "action") {
        Some(raw) => raw.parse()?,
        None => Action::List,
    };
    match action {
        Action::List => list(store),
        Action::Get => get(store, &required(args, "project")?),
        Action::History => history(store, args),
        Action::Proposals => proposals(store),
        Action::Confirm => {
            let outcome = project_proposals::confirm(store, &required(args, "key")?, &actor(args))?;
            Ok(json!({ "confirmed": outcome_json(&outcome) }))
        }
        Action::Reject => {
            let key = required(args, "key")?;
            let id = project_proposals::reject(store, &key, &actor(args))?;
            Ok(json!({ "rejected": { "key": key, "memory_id": id } }))
        }
        Action::Set => set(store, args),
        Action::Revert => revert(store, args),
        Action::Backfill => backfill(store, args),
    }
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn required(args: &Value, key: &str) -> Result<String> {
    text(args, key).ok_or_else(|| StoreError::Invalid(format!("'{key}' is required")))
}

/// Who is answering. Defaults to `human`: every write here is a confirmation,
/// and a caller that does not say otherwise is relaying a person's decision.
fn actor(args: &Value) -> String {
    text(args, "confirmed_by")
        .or_else(|| text(args, "by"))
        .unwrap_or_else(|| "human".to_string())
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn list(store: &Store<'_>) -> Result<Value> {
    let listed = project::list(store)?;
    let mut by_stage: Map<String, Value> = Map::new();
    for stage in [Stage::Idea, Stage::Designed, Stage::Built] {
        let rows: Vec<Value> = listed
            .projects
            .iter()
            .filter(|p| p.stage.value == stage)
            .map(compact)
            .collect();
        if !rows.is_empty() {
            by_stage.insert(stage.as_str().to_string(), Value::Array(rows));
        }
    }
    Ok(json!({
        "count": listed.projects.len(),
        "by_stage": by_stage,
        "invalid": listed.invalid,
    }))
}

/// One row of `list`: what a project is and where it lives, no evidence ids.
fn compact(project: &Project) -> Value {
    let mut row = json!({
        "name": project.entity.name,
        "status": project.status.value.as_str(),
        "evidence_count": project.stage.evidence.ids().len(),
    });
    let object = row.as_object_mut().expect("built as an object");
    if !project.entity.aliases.is_empty() {
        object.insert("aliases".into(), json!(project.entity.aliases));
    }
    let anchors = &project.anchors;
    let mut add = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            object.insert(key.into(), json!(value));
        }
    };
    add(
        "repo",
        anchors.repo.as_ref().map(|c| c.value.as_str().to_string()),
    );
    add(
        "path",
        anchors.path.as_ref().map(|c| c.value.as_str().to_string()),
    );
    add(
        "tracker",
        anchors
            .tracker
            .as_ref()
            .map(|c| c.value.as_str().to_string()),
    );
    add(
        "parent",
        anchors
            .parent
            .as_ref()
            .map(|c: &Claim<String>| c.value.clone()),
    );
    row
}

fn get(store: &Store<'_>, name: &str) -> Result<Value> {
    // A name nobody has claimed is a valid answer, not an error — the same
    // call that asks "what is this?" is how a caller learns it is unknown.
    match project::get(store, name)? {
        Some(project) => {
            let mut payload = serde_json::to_value(&project).unwrap_or_else(|_| json!({}));
            if let Some(object) = payload.as_object_mut() {
                object.insert("found".into(), json!(true));
            }
            Ok(payload)
        }
        None => Ok(json!({ "found": false, "name": name })),
    }
}

fn history(store: &Store<'_>, args: &Value) -> Result<Value> {
    let name = required(args, "project")?;
    let predicate = text(args, "predicate")
        .map(|raw| {
            raw.parse::<Predicate>()
                .or_else(|_| Predicate::from_qualified(&raw))
        })
        .transpose()?;
    Ok(json!({
        "project": name,
        "claims": project::history(store, &name, predicate)?,
    }))
}

fn proposals(store: &Store<'_>) -> Result<Value> {
    let open = project_proposals::proposals(store)?;
    Ok(json!({ "count": open.len(), "proposals": open }))
}

fn set(store: &Store<'_>, args: &Value) -> Result<Value> {
    let project = required(args, "project")?;
    let mut claims = Vec::new();
    for predicate in Predicate::ALL {
        if let Some(raw) = text(args, predicate.as_str()) {
            claims.push(ClaimValue::parse(*predicate, &raw)?);
        }
    }
    if claims.is_empty() {
        return Err(StoreError::Invalid(format!(
            "give at least one of: {}",
            Predicate::ALL
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let outcome = project::set(
        store,
        &project::SetInput {
            project,
            claims,
            evidence: string_list(args, "evidence"),
            because: text(args, "because"),
            confirmed_by: actor(args),
            proposal: text(args, "proposal"),
        },
    )?;
    Ok(json!({ "set": outcome_json(&outcome) }))
}

fn revert(store: &Store<'_>, args: &Value) -> Result<Value> {
    let outcome = project::revert(
        store,
        &project::RevertInput {
            project: required(args, "project")?,
            claim_id: required(args, "claim_id")?,
            because: required(args, "because")?,
            confirmed_by: actor(args),
        },
    )?;
    Ok(json!({ "reverted": outcome_json(&outcome) }))
}

fn backfill(store: &Store<'_>, args: &Value) -> Result<Value> {
    let root = std::path::PathBuf::from(required(args, "repo_root")?);
    let repo = project::RepoUrl::parse(&required(args, "repo")?)?;
    // Default off: a backfill writes, and the dry run is what a caller
    // should see first.
    let write = args.get("write").and_then(Value::as_bool).unwrap_or(false);
    let report = project_backfill::backfill(store, &root, &repo, write)?;
    Ok(json!({ "backfill": report }))
}

fn outcome_json(outcome: &WriteOutcome) -> Value {
    json!({
        "project": outcome.project,
        "written": outcome.written,
        "unchanged": outcome.unchanged.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "superseded": outcome.superseded,
        "because_id": outcome.because_id,
    })
}

/// The tool's input schema, so the MCP server and the docs cannot drift.
pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": Action::ALL.map(Action::as_str),
                "default": Action::List.as_str(),
                "description": "list: every project by stage with its anchors. get: one project with its evidence. history: a project's claim chain. proposals: what the store suggests, for a person to answer. confirm/reject: answer a proposal by key. set: record claims directly. revert: undo a claim with a compensating claim. backfill: read a checkout and seed evidence."
            },
            "project": { "type": "string", "description": "Project name or alias (get, history, set, revert)" },
            "key": { "type": "string", "description": "Proposal key (confirm, reject)" },
            "predicate": {
                "type": "string",
                "enum": Predicate::ALL.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
                "description": "Narrow history to one claim predicate"
            },
            "stage": { "type": "string", "enum": Stage::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>() },
            "status": { "type": "string", "enum": Status::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>() },
            "repo": { "type": "string", "description": "Repository URL (set, backfill)" },
            "path": { "type": "string", "description": "Repository-relative path, e.g. crates/apps/rusty_mux" },
            "tracker": { "type": "string", "description": "rusty_tick list, as tick:list:<uuid>" },
            "parent": { "type": "string", "description": "Parent project's name" },
            "evidence": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Memory ids supporting the claims; required unless 'because' is given"
            },
            "because": { "type": "string", "description": "Why, in one sentence. Saved as a memory and cited as the evidence. Required for revert." },
            "claim_id": { "type": "string", "description": "The current claim to undo (revert)" },
            "confirmed_by": { "type": "string", "default": "human", "description": "Who decided" },
            "repo_root": { "type": "string", "description": "Checkout to read (backfill)" },
            "write": { "type": "boolean", "default": false, "description": "backfill: save the evidence memories instead of only reporting them" }
        }
    })
}

/// What the tool tells a model it is for.
pub const DESCRIPTION: &str = "Projects as records, not names: what stage each project is at (idea, designed, built), whether it is active, dormant or abandoned, and where it lives (repository, path in the monorepo, rusty_tick list, parent project). Every claim cites the memories behind it, so 'is there actually a repo?' has an answer you can check. Start with action=list. Proposals are suggestions from captured session context that only a person confirms; nothing changes stage on its own.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;
    use crate::entity::{link_memory_entity, upsert_entity};
    use crate::models::EntityInput;

    fn seeded(store: &Store<'_>) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let entity = upsert_entity(
            store,
            &EntityInput {
                name: "rusty_mux".into(),
                kind: None,
                aliases: vec!["mux".into()],
            },
        )
        .unwrap();
        let row = NewMemory::new("mem_seen", "we talked about the mux", &now);
        Memories::new(store).insert(&row).unwrap();
        link_memory_entity(store, &row.id, &entity.id).unwrap();
        row.id
    }

    fn created(store: &Store<'_>) -> Value {
        let evidence = seeded(store);
        call(
            store,
            &json!({ "action": "set", "project": "rusty_mux", "stage": "idea",
                     "status": "active", "evidence": [evidence] }),
        )
        .unwrap()
    }

    #[test]
    fn actions_parse_and_say_which_write() {
        assert_eq!("  LIST ".parse::<Action>().unwrap(), Action::List);
        assert!("explode".parse::<Action>().is_err());
        assert!(Action::Set.writes() && Action::Revert.writes());
        assert!(!Action::List.writes() && !Action::History.writes());
    }

    #[test]
    fn list_defaults_and_stays_compact() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        created(&store);
        call(
            &store,
            &json!({ "action": "set", "project": "mux",
                     "repo": "git@github.com:Rusty-Mill/rusty_mux.git",
                     "because": "it has its own repo now" }),
        )
        .unwrap();

        // No action at all is a listing: the cheapest call a session makes.
        let listed = call(&store, &json!({})).unwrap();
        assert_eq!(listed["count"], 1);
        let row = &listed["by_stage"]["idea"][0];
        assert_eq!(row["name"], "rusty_mux");
        assert_eq!(row["status"], "active");
        assert_eq!(row["repo"], "https://github.com/Rusty-Mill/rusty_mux");
        assert_eq!(row["aliases"][0], "mux");
        assert_eq!(row["evidence_count"], 1);
        assert!(row.get("evidence").is_none(), "ids come from get, not list");
    }

    #[test]
    fn get_answers_an_unknown_name_without_erroring() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        created(&store);
        assert_eq!(
            call(&store, &json!({ "action": "get", "project": "mux" })).unwrap()["found"],
            true
        );
        let missing = call(&store, &json!({ "action": "get", "project": "ghost" })).unwrap();
        assert_eq!(missing["found"], false);
        assert!(
            call(&store, &json!({ "action": "get" })).is_err(),
            "project is required"
        );
    }

    #[test]
    fn history_takes_a_bare_or_qualified_predicate() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        created(&store);
        call(
            &store,
            &json!({ "action": "set", "project": "rusty_mux", "stage": "built",
                     "because": "the repo exists" }),
        )
        .unwrap();
        for predicate in ["stage", "project.stage"] {
            let shown = call(
                &store,
                &json!({ "action": "history", "project": "rusty_mux", "predicate": predicate }),
            )
            .unwrap();
            let values: Vec<&str> = shown["claims"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["value"].as_str().unwrap())
                .collect();
            assert_eq!(values, ["idea", "built"], "{predicate}");
        }
        assert!(call(
            &store,
            &json!({ "action": "history", "project": "rusty_mux", "predicate": "owner" })
        )
        .is_err());
    }

    #[test]
    fn set_refuses_a_call_with_no_claims_and_reports_what_changed() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let outcome = created(&store);
        assert_eq!(outcome["set"]["written"].as_array().unwrap().len(), 2);
        assert!(call(&store, &json!({ "action": "set", "project": "rusty_mux" })).is_err());

        let again = call(
            &store,
            &json!({ "action": "set", "project": "rusty_mux", "stage": "idea", "because": "same" }),
        )
        .unwrap();
        assert!(again["set"]["written"].as_array().unwrap().is_empty());
        assert_eq!(again["set"]["unchanged"][0], "stage");
    }

    #[test]
    fn revert_and_proposals_round_trip_through_the_tool() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        created(&store);
        let built = call(
            &store,
            &json!({ "action": "set", "project": "rusty_mux", "stage": "built", "because": "wrong" }),
        )
        .unwrap();
        let claim_id = built["set"]["written"][0].as_str().unwrap().to_string();

        assert!(
            call(
                &store,
                &json!({ "action": "revert", "project": "rusty_mux", "claim_id": claim_id })
            )
            .is_err(),
            "revert needs a because"
        );
        let reverted = call(
            &store,
            &json!({ "action": "revert", "project": "rusty_mux", "claim_id": claim_id,
                     "because": "it was not built" }),
        )
        .unwrap();
        assert_eq!(reverted["reverted"]["project"], "rusty_mux");
        assert_eq!(
            call(&store, &json!({ "action": "get", "project": "rusty_mux" })).unwrap()["stage"]
                ["value"],
            "idea"
        );

        let open = call(&store, &json!({ "action": "proposals" })).unwrap();
        assert_eq!(open["count"], 0, "nothing to suggest for a settled project");
        assert!(call(
            &store,
            &json!({ "action": "confirm", "key": "nope:stage:built" })
        )
        .is_err());
    }

    #[test]
    fn backfill_defaults_to_a_dry_run() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let root = std::env::temp_dir().join(format!("rrm_tool_backfill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("crates").join("apps").join("rusty_mux");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        seeded(&store);

        let reported = call(
            &store,
            &json!({ "action": "backfill", "repo_root": root.display().to_string(),
                     "repo": "https://github.com/Rusty-Mill/rusty_mill" }),
        )
        .unwrap();
        assert_eq!(reported["backfill"]["dry_run"], true);
        assert_eq!(reported["backfill"]["seeds"][0]["project"], "rusty_mux");
        assert!(reported["backfill"]["seeds"][0].get("memory_id").is_none());
        assert!(call(
            &store,
            &json!({ "action": "backfill", "repo": "https://github.com/a/b" })
        )
        .is_err());
    }

    #[test]
    fn the_schema_advertises_every_action_and_predicate() {
        let schema = input_schema();
        let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
        assert_eq!(actions.len(), Action::ALL.len());
        let predicates = schema["properties"]["predicate"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(predicates.len(), Predicate::ALL.len());
        for predicate in Predicate::ALL {
            assert!(
                schema["properties"].get(predicate.as_str()).is_some(),
                "{} has no argument",
                predicate.as_str()
            );
        }
    }
}
