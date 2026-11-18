//! Dry-run plans for card #1564: parsing decision logs, and planning the
//! normalise and import passes. Nothing here writes to a store.
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::decisions_backfill::{parse_log, plan_import, plan_normalize, render_import};
use remind_me_core::models::Memory;
use remind_me_core::Database;
use serde_json::json;

const LOG: &str = "\
# Decisions

| # | Decision | Why | Rejected | Tag |
|---|---|---|---|---|
| D1 | One memory for every agent | The purpose | Per-client stores | [stated] |
| D2 | Keep \\| pipes | Escaped | — | [default] |

| D3 | Supersedes D2: drop pipes | Simpler | — | [stated] |
| ~~D4~~ | ~~Old idea~~ | Dropped | — | [default] |
| D5 | Confirms D1 | Quote found | — | [stated] |
| D6 | Supersedes the Millwright D74 | Voice | — | [stated] |
| D7 | No rationale here |  | — | [default] |
| D1 | Twice | Dup | — | [default] |

## Scoring

| Outcome | Points |
|---|---|
| Accept | 8 |

## D8. Lenses, not stages

**Decision.** Roles become lenses.
**Why.** Handoffs only add queues.

## Notes

Supersedes nothing; this prose is not a section.
";

#[test]
fn parse_reads_tables_sections_and_reports_the_rest() {
    let log = parse_log(LOG);
    let refs: Vec<&str> = log
        .entries
        .iter()
        .map(|e| e.decision_ref.as_str())
        .collect();
    assert_eq!(refs, ["D1", "D2", "D3", "D4", "D5", "D6", "D7", "D1", "D8"]);
    assert_eq!(log.entries[1].title, "Keep | pipes");
    assert!(log.entries[3].struck);
    let d8 = &log.entries[8];
    assert_eq!(d8.title, "Lenses, not stages");
    assert_eq!(d8.rationale.as_deref(), Some("Handoffs only add queues."));
    assert_eq!(log.unparsed.len(), 1, "the scoring table has no ref column");
}

#[test]
fn slug_keys_are_refs_and_dated_rows_are_unparsed() {
    let slugs = parse_log("| Key | Decision | Why | Date |\n|---|---|---|---|\n| entry-rule | Notes first | Less noise | 2026-11-12 |\n");
    assert_eq!(slugs.entries[0].decision_ref, "entry-rule");
    assert_eq!(slugs.entries[0].date.as_deref(), Some("2026-11-12"));
    let dated =
        parse_log("| Date | Decision | Why |\n|---|---|---|\n| 2026-10-21 | Build fresh | Gap |\n");
    assert!(dated.entries.is_empty());
    assert_eq!(dated.unparsed[0].reason, "no ref column");
}

#[test]
fn a_ref_in_both_an_index_table_and_a_section_is_read_once_from_the_section() {
    let log = parse_log(
        "| # | Decision |\n|---|---|\n| D1 | Short |\n\n## D1. Long title\n\n**Why.** Because.\n",
    );
    assert_eq!(log.entries.len(), 1);
    assert_eq!(log.entries[0].title, "Long title");
}

fn decision(id: &str, project: &str, content: &str, metadata: serde_json::Value) -> NewMemory {
    let mut row = NewMemory::new(id, content, "2026-11-01T00:00:00Z");
    row.memory_type = "decision".into();
    row.category = "decision".into();
    row.project = Some(project.into());
    row.metadata = metadata;
    row
}

fn stored(rows: Vec<NewMemory>) -> Vec<Memory> {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    let memories = Memories::new(&store);
    for row in &rows {
        memories.insert(row).expect("insert");
    }
    memories.all_live().expect("read")
}

#[test]
fn import_sorts_entries_into_buckets_and_states_links() {
    let memories = stored(vec![
        decision(
            "m-d2",
            "mill",
            "rusty D2: keep | pipes",
            json!({"decision_ref": "D2", "rationale": "x"}),
        ),
        decision(
            "m-d3",
            "mill",
            "something else",
            json!({"decision_id": "D3", "rationale": "x"}),
        ),
        decision(
            "m-other",
            "tick",
            "D5: elsewhere",
            json!({"decision_ref": "D5", "rationale": "x"}),
        ),
    ]);
    let plan = plan_import(&parse_log(LOG), "mill", "DECISIONS.md", &memories);
    let refs = |rows: &[remind_me_core::decisions_backfill::ImportRow]| {
        rows.iter()
            .map(|r| r.decision_ref.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        refs(&plan.new),
        ["D5", "D6", "D8"],
        "another project's D5 does not match"
    );
    assert_eq!(refs(&plan.unchanged), ["D2"]);
    assert_eq!(refs(&plan.changed), ["D3"]);
    assert_eq!(refs(&plan.needs_rationale), ["D7"]);
    assert_eq!(refs(&plan.conflicts), ["D1", "D4", "D1"]);
    assert_eq!(plan.supersedes.len(), 1);
    assert_eq!(
        (
            plan.supersedes[0].decision_ref.as_str(),
            plan.supersedes[0].supersedes.as_str()
        ),
        ("D3", "D2")
    );
    assert_eq!(plan.confirms[0].supersedes, "D1");
    assert_eq!(refs(&plan.ambiguous), ["D6"]);
    let report = render_import(&plan);
    assert!(report.contains("Nothing was written."));
    assert_eq!(report, render_import(&plan), "deterministic");
}

#[test]
fn normalize_proposes_only_what_it_can_back() {
    let mut legacy = NewMemory::new(
        "m-legacy",
        "Use systemd for the daemon",
        "2026-09-01T00:00:00Z",
    );
    legacy.category = "decision".into();
    legacy.metadata = json!({"rationale": "Already on the host"});
    legacy.project = Some("homelab".into());
    let mut bare = NewMemory::new("m-bare", "Pick Postgres", "2026-09-02T00:00:00Z");
    bare.category = "decision".into();
    bare.project = Some("homelab".into());
    let mut defines = NewMemory::new(
        "m-def",
        "Let Em Loose D50 [default]: one writer",
        "2026-09-03T00:00:00Z",
    );
    defines.category = "fact".into();
    defines.project = Some("let_em_loose".into());
    defines.metadata = json!({"rationale": "one writer"});
    let mut wrong = NewMemory::new(
        "m-wrong",
        "rusty_term D9 [stated]: x",
        "2026-09-04T00:00:00Z",
    );
    wrong.project = Some("mill".into());
    let mut mention = NewMemory::new(
        "m-mention",
        "Followed D50 when merging",
        "2026-09-05T00:00:00Z",
    );
    mention.memory_type = "work_log".into();
    mention.project = Some("let-em-loose".into());
    let mut secret = NewMemory::new("m-secret", "D1: secret", "2026-09-06T00:00:00Z");
    secret.sensitive = true;
    let memories = stored(vec![
        legacy,
        bare,
        defines,
        wrong,
        mention,
        secret,
        decision(
            "m-id",
            "let-em-loose",
            "orch rule",
            json!({"decision_id": "D136", "rationale": "x"}),
        ),
        decision(
            "m-named",
            "rusty_term",
            "x",
            json!({"decision": "rusty_term D64", "rationale": "x"}),
        ),
        decision(
            "m-done",
            "mill",
            "x",
            json!({"decision_ref": "D1", "rationale": "x"}),
        ),
    ]);

    let plan = plan_normalize(&memories, None);
    let ids = |rows: &[remind_me_core::decisions_backfill::NormalizeRow]| {
        rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&plan.retype), ["m-legacy", "m-def"]);
    assert_eq!(plan.retype[1].decision_ref.as_deref(), Some("D50"));
    assert_eq!(ids(&plan.add_ref), ["m-id", "m-named"]);
    assert_eq!(plan.add_ref[1].decision_ref.as_deref(), Some("D64"));
    assert_eq!(ids(&plan.needs_rationale), ["m-bare"]);
    assert_eq!(ids(&plan.conflicts), ["m-wrong"]);
    assert_eq!(ids(&plan.mentions), ["m-mention"]);

    let scoped = plan_normalize(&memories, Some("Let Em Loose"));
    assert_eq!(ids(&scoped.retype), ["m-def"]);
    assert_eq!(ids(&scoped.add_ref), ["m-id"]);
}
