//! Dead ends that warn before they're repeated (card #1484, pack D115-D117,
//! D123): written with their fields checked, then surfaced in search, the
//! brief and the prompt hook, outside the ranking.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::context_brief::{self, ContextRequest, Mode, Outcome};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::{Store, StoreError};
use remind_me_core::kinds::StructuredFields;
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use serde_json::{json, Value};

const TRIED: &str = "rebuild the musl target with jemalloc";

fn put(
    store: &Store<'_>,
    id: &str,
    kind: &str,
    content: &str,
    metadata: Value,
    project: Option<&str>,
) {
    let at = chrono::Utc::now().to_rfc3339();
    let mut row = NewMemory::new(id, content, &at);
    row.memory_type = kind.into();
    row.metadata = metadata;
    row.project = project.map(str::to_string);
    Memories::new(store).insert(&row).unwrap();
}

fn dead_end(store: &Store<'_>, id: &str, project: Option<&str>) {
    let meta =
        json!({ "tried": TRIED, "why_failed": "the allocator segfaults under musl", "cost": 90 });
    put(store, id, "dead_end", TRIED, meta, project);
}

fn add(store: &Store<'_>, metadata: Value) -> Result<remind_me_core::Memory, StoreError> {
    queries::add_memory_with(
        store,
        MemoryAddInput {
            content: "a dead end".into(),
            category: "general".into(),
            source: "manual".into(),
            metadata,
            project: Some("alpha".into()),
            ..Default::default()
        },
        &StructuredFields {
            memory_type: Some("dead_end".into()),
            ..StructuredFields::default()
        },
    )
}

fn search(
    store: &Store<'_>,
    query: &str,
    project: Option<&str>,
) -> remind_me_core::expansion::MemorySearchResponse {
    let mut input = MemorySearchInput {
        query: query.into(),
        limit: 1,
        ..MemorySearchInput::default()
    };
    input.scope.project = project.map(str::to_string);
    queries::search_with_expansions(store, &input).unwrap()
}

#[test]
fn a_dead_end_needs_tried_and_why_failed_and_a_sane_cost() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    assert!(add(
        &store,
        json!({ "tried": "x", "why_failed": "y", "cost": "high" })
    )
    .is_ok());
    assert!(add(
        &store,
        json!({ "tried": "x", "why_failed": "y", "cost": 15 })
    )
    .is_ok());
    for (meta, says) in [
        (json!({ "why_failed": "y" }), "tried"),
        (json!({ "tried": " ", "why_failed": "y" }), "tried"),
        (json!({ "tried": "x" }), "why_failed"),
        (
            json!({ "tried": "x", "why_failed": "y", "cost": 0 }),
            "cost",
        ),
        (
            json!({ "tried": "x", "why_failed": "y", "cost": "huge" }),
            "cost",
        ),
        (
            json!({ "tried": "x".repeat(1001), "why_failed": "y" }),
            "tried",
        ),
    ] {
        let err = add(&store, meta.clone()).unwrap_err().to_string();
        assert!(err.contains(says), "{meta}: {err}");
    }
}

#[test]
fn from_must_name_a_live_memory() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    put(
        &store,
        "mem_decision",
        "decision",
        "use jemalloc",
        json!({ "rationale": "speed" }),
        None,
    );
    let ok = add(
        &store,
        json!({ "tried": "x", "why_failed": "y", "from": "mem_decision" }),
    );
    assert!(ok.is_ok(), "{ok:?}");
    let err = add(
        &store,
        json!({ "tried": "x", "why_failed": "y", "from": "mem_gone" }),
    )
    .unwrap_err();
    assert!(err.to_string().contains("mem_gone"), "{err}");
}

#[test]
fn a_search_repeating_a_dead_end_warns_outside_the_ranking() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    dead_end(&store, "dead", Some("alpha"));
    put(
        &store,
        "plain",
        "fact",
        "musl builds are reproducible with jemalloc off",
        json!({}),
        Some("alpha"),
    );

    let res = search(&store, "rebuild musl with jemalloc", Some("alpha"));
    let warned: Vec<&str> = res.warnings.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(warned, ["dead"]);
    assert_eq!(res.warnings[0].cost, Some(json!(90)));
    assert!(
        res.memories.iter().all(|r| r.memory.id != "dead"),
        "a warned dead end is not repeated among the hits"
    );
    let hits: Vec<&str> = res.memories.iter().map(|r| r.memory.id.as_str()).collect();
    assert_eq!(
        hits,
        ["plain"],
        "the warned dead end leaves its slot to the next hit"
    );
    assert_eq!(res.returned, 1);
}

#[test]
fn no_match_other_projects_and_sensitive_ones_give_no_warning() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    dead_end(&store, "theirs", Some("beta"));
    assert!(search(&store, "write the tuesday release notes", None)
        .warnings
        .is_empty());
    assert!(search(&store, "rebuild musl with jemalloc", Some("alpha"))
        .warnings
        .is_empty());

    let at = chrono::Utc::now().to_rfc3339();
    let mut secret = NewMemory::new("secret", TRIED, &at);
    secret.memory_type = "dead_end".into();
    secret.metadata = json!({ "tried": TRIED, "why_failed": "x" });
    secret.sensitive = true;
    Memories::new(&store).insert(&secret).unwrap();
    assert!(search(&store, "rebuild musl with jemalloc", Some("alpha"))
        .warnings
        .is_empty());
}

#[test]
fn search_warnings_are_capped_and_an_empty_query_has_none() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..5 {
        dead_end(&store, &format!("d{i}"), None);
    }
    let res = search(&store, "rebuild musl with jemalloc", None);
    assert_eq!(res.warnings.len(), remind_me_core::dead_ends::SEARCH_LIMIT);
    assert_eq!(res.warnings_omitted, 2);
    assert!(search(&store, "", None).warnings.is_empty());
}

fn brief(
    store: &Store<'_>,
    mode: Mode,
    project: Option<&str>,
    prompt: Option<&str>,
) -> context_brief::Brief {
    let request = ContextRequest {
        mode,
        project,
        prompt,
        budget: 0,
        ..ContextRequest::default()
    };
    match context_brief::build(store, &request).unwrap() {
        Outcome::Brief(brief) => brief,
        other => panic!("{other:?}"),
    }
}

fn section<'a>(brief: &'a context_brief::Brief, name: &str) -> Vec<&'a str> {
    brief
        .sections
        .iter()
        .filter(|s| s.name == name)
        .flat_map(|s| s.items.iter().map(|i| i.id.as_str()))
        .collect()
}

#[test]
fn a_project_brief_lists_its_dead_ends_first_and_not_again_in_activity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    dead_end(&store, "ours", Some("alpha"));
    dead_end(&store, "theirs", Some("beta"));
    put(
        &store,
        "note",
        "fact",
        "alpha ships weekly",
        json!({}),
        Some("alpha"),
    );

    let b = brief(&store, Mode::Project, Some("alpha"), None);
    assert_eq!(b.sections[0].name, "warnings");
    assert_eq!(section(&b, "warnings"), ["ours"]);
    assert!(!section(&b, "activity").contains(&"ours"));
    assert!(context_brief::render_markdown(&b).contains("Dead ends"));
}

#[test]
fn the_prompt_hook_warns_only_when_the_prompt_repeats_one() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    dead_end(&store, "dead", None);
    let hit = brief(
        &store,
        Mode::Prompt,
        None,
        Some("let's rebuild musl with jemalloc again"),
    );
    assert_eq!(section(&hit, "warnings"), ["dead"]);
    assert!(!section(&hit, "hits").contains(&"dead"));
    let miss = brief(
        &store,
        Mode::Prompt,
        None,
        Some("draft the weekly status update"),
    );
    assert!(section(&miss, "warnings").is_empty());
}

#[test]
fn the_prompt_hook_caps_its_warnings() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..4 {
        dead_end(&store, &format!("d{i}"), None);
    }
    let b = brief(
        &store,
        Mode::Prompt,
        None,
        Some("rebuild musl with jemalloc"),
    );
    let warnings = b.sections.iter().find(|s| s.name == "warnings").unwrap();
    assert_eq!(
        warnings.items.len(),
        remind_me_core::dead_ends::PROMPT_LIMIT
    );
    assert_eq!(warnings.omitted, 2);
}

#[test]
fn a_brief_warning_keeps_why_it_failed_when_tried_is_long() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let tried = format!("{}\nsecond line", "x".repeat(400));
    let meta = json!({ "tried": tried, "why_failed": "it leaked" });
    put(&store, "long", "dead_end", "a long dead end", meta, None);
    let b = brief(&store, Mode::Session, None, None);
    let warning = b
        .sections
        .iter()
        .find(|s| s.name == "warnings")
        .and_then(|s| s.items.first())
        .unwrap();
    assert!(
        warning.text.contains("failed: it leaked"),
        "{}",
        warning.text
    );
    assert!(!warning.text.contains("second line"));
}

#[test]
fn a_dead_end_a_stronger_statement_overrules_is_not_a_warning() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let at = chrono::Utc::now().to_rfc3339();
    let mut weak = NewMemory::new("weak", TRIED, &at);
    weak.memory_type = "dead_end".into();
    weak.metadata = json!({ "tried": TRIED, "why_failed": "segfaults" });
    weak.subject = Some("build".into());
    weak.predicate = Some("allocator".into());
    weak.object = Some("jemalloc".into());
    weak.confidence = 0.4;
    Memories::new(&store).insert(&weak).unwrap();
    let mut strong = NewMemory::new("strong", "the build uses mimalloc", &at);
    strong.memory_type = "fact".into();
    strong.subject = Some("build".into());
    strong.predicate = Some("allocator".into());
    strong.object = Some("mimalloc".into());
    strong.project = Some("alpha".into());
    Memories::new(&store).insert(&strong).unwrap();

    let b = brief(&store, Mode::Project, Some("alpha"), None);
    assert!(
        section(&b, "warnings").is_empty(),
        "{:?}",
        section(&b, "warnings")
    );
}

#[test]
fn an_inference_a_projectless_dead_end_overrules_leaves_activity_too() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let at = chrono::Utc::now().to_rfc3339();
    let mut dead = NewMemory::new("dead", TRIED, &at);
    dead.memory_type = "dead_end".into();
    dead.metadata = json!({ "tried": TRIED, "why_failed": "segfaults" });
    dead.subject = Some("build".into());
    dead.predicate = Some("allocator".into());
    dead.object = Some("not jemalloc".into());
    Memories::new(&store).insert(&dead).unwrap();
    let mut guess = NewMemory::new("guess", "the build probably uses jemalloc", &at);
    guess.memory_type = "fact".into();
    guess.subject = Some("build".into());
    guess.predicate = Some("allocator".into());
    guess.object = Some("jemalloc".into());
    guess.confidence = 0.4;
    guess.project = Some("alpha".into());
    Memories::new(&store).insert(&guess).unwrap();

    let b = brief(&store, Mode::Project, Some("alpha"), None);
    assert_eq!(section(&b, "warnings"), ["dead"]);
    let shown: Vec<&str> = b
        .sections
        .iter()
        .flat_map(|s| s.items.iter().map(|i| i.id.as_str()))
        .collect();
    assert!(!shown.contains(&"guess"), "{shown:?}");
}

#[test]
fn the_prompt_hook_does_not_warn_with_a_dead_end_a_hit_overrules() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let at = chrono::Utc::now().to_rfc3339();
    let mut weak = NewMemory::new("weak", TRIED, &at);
    weak.memory_type = "dead_end".into();
    weak.metadata = json!({ "tried": TRIED, "why_failed": "segfaults" });
    weak.subject = Some("musl build".into());
    weak.predicate = Some("allocator".into());
    weak.object = Some("not jemalloc".into());
    weak.confidence = 0.4;
    Memories::new(&store).insert(&weak).unwrap();
    let mut strong = NewMemory::new(
        "strong",
        "rebuild the musl target with jemalloc: it works",
        &at,
    );
    strong.memory_type = "fact".into();
    strong.subject = Some("musl build".into());
    strong.predicate = Some("allocator".into());
    strong.object = Some("jemalloc".into());
    Memories::new(&store).insert(&strong).unwrap();

    let b = brief(
        &store,
        Mode::Prompt,
        None,
        Some("rebuild musl with jemalloc"),
    );
    assert!(
        section(&b, "warnings").is_empty(),
        "{:?}",
        section(&b, "warnings")
    );
    assert!(section(&b, "hits").contains(&"strong"));
}

#[test]
fn a_tight_project_brief_fits_a_long_warning_before_activity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let meta = json!({ "tried": "t".repeat(240), "why_failed": "w".repeat(240) });
    put(
        &store,
        "long",
        "dead_end",
        "a long dead end",
        meta,
        Some("alpha"),
    );
    for i in 0..5 {
        let content = format!("alpha note {i} {}", "n".repeat(200));
        put(
            &store,
            &format!("n{i}"),
            "fact",
            &content,
            json!({}),
            Some("alpha"),
        );
    }
    let request = ContextRequest {
        mode: Mode::Project,
        project: Some("alpha"),
        budget: 400,
        ..ContextRequest::default()
    };
    let Outcome::Brief(b) = context_brief::build(&store, &request).unwrap() else {
        panic!("unknown project");
    };
    assert_eq!(section(&b, "warnings"), ["long"]);
    assert!(b.budget.used <= 400);
}

#[test]
fn another_projects_dead_end_does_not_silence_a_prompt_warning() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let at = chrono::Utc::now().to_rfc3339();
    let rival = |id: &str, object: &str, confidence: f64, project: &str| {
        let mut row = NewMemory::new(id, TRIED, &at);
        row.memory_type = "dead_end".into();
        row.metadata = json!({ "tried": TRIED, "why_failed": "segfaults" });
        row.subject = Some("musl build".into());
        row.predicate = Some("allocator".into());
        row.object = Some(object.into());
        row.confidence = confidence;
        row.project = Some(project.into());
        Memories::new(&store).insert(&row).unwrap();
    };
    rival("ours", "not jemalloc", 0.4, "alpha");
    rival("theirs", "not mimalloc", 1.0, "beta");

    let b = brief(
        &store,
        Mode::Prompt,
        Some("alpha"),
        Some("rebuild musl with jemalloc"),
    );
    assert_eq!(section(&b, "warnings"), ["ours"]);
}
