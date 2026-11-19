//! Card #1564: parsing decision logs, planning the normalise and import
//! passes, and applying the normalise plan's `add_ref` rows.
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::decisions_backfill::{
    apply_normalize, apply_planned, parse_log, plan_import, plan_normalize, render_import,
};
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
            "mill D2: keep | pipes",
            json!({"decision_ref": "D2", "rationale": "Escaped."}),
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
    assert!(report.contains("Nothing was written to the store."));
    assert_eq!(report, render_import(&plan), "deterministic");
}

fn buckets(plan: &[remind_me_core::decisions_backfill::ImportRow]) -> Vec<(String, String)> {
    plan.iter()
        .map(|r| (r.decision_ref.clone(), r.note.clone()))
        .collect()
}

#[test]
fn a_changed_body_or_rationale_under_the_same_heading_is_changed() {
    let log = parse_log(
        "## D8. Lenses, not stages\n\n**Decision.** Roles become lenses,\nnot stages.\n**Why.** Handoffs add queues.\n**Rejected.** Keep stages.\n",
    );
    assert_eq!(
        log.entries[0].decision.as_deref(),
        Some("Roles become lenses, not stages.")
    );
    let why = json!({"decision_ref": "D8", "rationale": "Handoffs add queues"});
    let check = |content: &str, metadata: serde_json::Value| {
        let memories = stored(vec![decision("m-d8", "mill", content, metadata)]);
        let plan = plan_import(&log, "mill", "DECISIONS.md", &memories);
        (buckets(&plan.unchanged), buckets(&plan.changed))
    };
    let same = "D8 [stated]: Lenses, not stages. Roles become lenses, not stages.";
    assert_eq!(
        check(same, why.clone()).0.len(),
        1,
        "same words are unchanged"
    );
    let (unchanged, changed) = check(
        "D8: Lenses, not stages. Roles remain stages, not lenses.",
        why.clone(),
    );
    assert!(unchanged.is_empty());
    assert_eq!(changed, [("D8".into(), "decision text differs".into())]);
    let (_, changed) = check("D8: Lenses, not stages", why.clone());
    assert_eq!(
        changed[0].1, "decision text differs",
        "a stored heading alone is not the decision"
    );
    let (_, changed) = check(same, json!({"decision_ref": "D8", "rationale": "Speed"}));
    assert_eq!(changed[0].1, "same text, different rationale");
    let (_, changed) = check(same, json!({"decision_ref": "D8"}));
    assert_eq!(changed[0].1, "same text; the store has no rationale");
}

#[test]
fn a_table_title_inside_longer_stored_text_is_changed() {
    let log = parse_log("| # | Decision | Why |\n|---|---|---|\n| D2 | Use X | Fast |\n");
    let memories = stored(vec![decision(
        "m-d2",
        "mill",
        "D2: Use X and Y",
        json!({"decision_ref": "D2", "rationale": "Fast"}),
    )]);
    let plan = plan_import(&log, "mill", "DECISIONS.md", &memories);
    assert!(plan.unchanged.is_empty());
    assert_eq!(plan.changed.len(), 1);
}

#[test]
fn only_plain_links_on_eligible_entries_are_proposed() {
    let log = parse_log(concat!(
        "| # | Decision | Why |\n|---|---|---|\n",
        "| ~~D4~~ | ~~Supersedes D2~~ | Dropped |\n",
        "| D5 | Supersedes D1 | a |\n",
        "| D5 | Supersedes D3 | b |\n",
        "| D6 | This does not supersedes D2 | c |\n",
        "| D7 | Rejected wording was \"supersedes D2\" | d |\n",
        "| D8 | Supersedes D3 for that run only; nothing else | e |\n",
        "| D12 | Supersedes D3, D4 and D5's tag only | h |\n",
        "| D9 | Keep it (supersedes D1). Run only once | f |\n",
        "| D11 | Supersedes D2 in part. Keep the rest | g |\n",
        "\n## D10. Keep both\n\n**Decision.** Keep both.\n**Rejected.** Supersedes D9.\n",
    ));
    let plan = plan_import(&log, "mill", "DECISIONS.md", &[]);
    let links: Vec<(&str, &str)> = plan
        .supersedes
        .iter()
        .map(|l| (l.decision_ref.as_str(), l.supersedes.as_str()))
        .collect();
    assert_eq!(links, [("D9", "D1")], "only D9's plain link is proposed");
    assert_eq!(
        buckets(&plan.ambiguous),
        [
            ("D4".into(), "link wording on a conflicting entry".into()),
            ("D5".into(), "link wording on a conflicting entry".into()),
            ("D5".into(), "link wording on a conflicting entry".into()),
            ("D6".into(), "negated link".into()),
            ("D7".into(), "quoted link".into()),
            ("D8".into(), "partial link (words follow the ref)".into()),
            ("D12".into(), "partial link (the clause says only)".into()),
            ("D11".into(), "partial link (words follow the ref)".into()),
        ],
        "a link in Rejected is not the entry's own"
    );
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

#[test]
fn normalize_reports_refs_that_disagree_instead_of_picking_one() {
    let memories = stored(vec![
        decision(
            "m-split",
            "mill",
            "D8: lenses",
            json!({"decision_id": "D9", "rationale": "x"}),
        ),
        decision(
            "m-elsewhere",
            "mill",
            "x",
            json!({"decision": "other-project D9", "rationale": "x"}),
        ),
        decision(
            "m-agree",
            "mill",
            "D9: x",
            json!({"decision": "Mill D9", "rationale": "x"}),
        ),
    ]);
    let plan = plan_normalize(&memories, None);
    let notes: Vec<(&str, &str)> = plan
        .conflicts
        .iter()
        .map(|r| (r.id.as_str(), r.note.as_str()))
        .collect();
    assert_eq!(
        notes,
        [
            (
                "m-elsewhere",
                "metadata.decision names project \"other-project\", field is \"mill\""
            ),
            (
                "m-split",
                "refs disagree: content D8, metadata.decision_id D9"
            ),
        ]
    );
    assert_eq!(plan.add_ref.len(), 1);
    assert_eq!(plan.add_ref[0].id, "m-agree");
}

#[test]
fn the_import_report_carries_each_entry_in_full() {
    let body =
        |text: &str| format!("## D8. Same heading\n\n**Decision.** {text}\n**Why.** Because.\n");
    let long = "word ".repeat(40);
    let plans: Vec<String> = [
        body("Roles become lenses."),
        body("Roles stay stages."),
        body(&long),
    ]
    .iter()
    .map(|log| render_import(&plan_import(&parse_log(log), "mill", "DECISIONS.md", &[])))
    .collect();
    assert_ne!(plans[0], plans[1], "different bodies render differently");
    assert!(plans[0].contains("  - Decision: Roles become lenses."));
    assert!(plans[0].contains("  - Why: Because."));
    assert!(plans[2].contains(long.trim()), "nothing is cut");

    let memories = stored(vec![decision(
        "m-d8",
        "mill",
        "D8: Same heading. Roles stay stages.",
        json!({"decision_ref": "D8", "rationale": "Old reason"}),
    )]);
    let plan = plan_import(
        &parse_log(&body("Roles become lenses.")),
        "mill",
        "x",
        &memories,
    );
    let changed = &plan.changed[0];
    assert_eq!(
        changed.stored.as_deref(),
        Some("D8: Same heading. Roles stay stages.")
    );
    assert_eq!(changed.stored_rationale.as_deref(), Some("Old reason"));
    assert!(render_import(&plan).contains("  - Stored why: Old reason"));
}

#[test]
fn signs_and_comparisons_count_markup_does_not() {
    let table = |decision: &str, why: &str| {
        parse_log(&format!(
            "| # | Decision | Why |\n|---|---|---|\n| D2 | {decision} | {why} |\n"
        ))
    };
    let bucket = |log: &remind_me_core::decisions_backfill::ParsedLog, content: &str, why: &str| {
        let memories = stored(vec![decision(
            "m-d2",
            "mill",
            content,
            json!({"decision_ref": "D2", "rationale": why}),
        )]);
        let plan = plan_import(log, "mill", "DECISIONS.md", &memories);
        match (plan.unchanged.len(), plan.changed.first()) {
            (1, None) => "unchanged".to_string(),
            (_, Some(row)) => row.note.clone(),
            _ => "neither".to_string(),
        }
    };
    let same = "Same reason";
    assert_eq!(
        bucket(&table("timeout -1", same), "D2: timeout 1", same),
        "decision text differs"
    );
    assert_eq!(
        bucket(&table("latency < 100", same), "D2: latency > 100", same),
        "decision text differs"
    );
    assert_eq!(
        bucket(
            &table("Use X", "keeps p95 < 100"),
            "D2: Use X",
            "keeps p95 > 100"
        ),
        "same text, different rationale"
    );
    assert_eq!(
        bucket(&table("Use X", "retry -1"), "D2: Use X", "retry 1"),
        "same text, different rationale"
    );
    assert_eq!(
        bucket(&table("**Use** X.", same), "D2: use x", same),
        "unchanged"
    );
    for (file, stored) in [("*.rs", "rs"), ("a \\| b", "a b"), ("f(x)", "f x")] {
        assert_eq!(
            bucket(&table(file, same), &format!("D2: {stored}"), same),
            "decision text differs",
            "{file} vs {stored}"
        );
    }
    assert_eq!(
        bucket(&table("bold", same), "D2: **bold**", same),
        "unchanged",
        "paired markup is not a change"
    );
    assert_eq!(
        bucket(&table("Use X", "a \\| b"), "D2: Use X", "a b"),
        "same text, different rationale"
    );
    assert_eq!(
        bucket(
            &table("Use X", same),
            "let-em-loose D2 [stated] — Use X",
            same
        ),
        "decision text differs",
        "a different project qualifier must remain in the comparison"
    );
}

#[test]
fn only_valid_matching_definition_prefixes_are_removed() {
    let log = parse_log("| # | Decision | Why |\n|---|---|---|\n| D2 | use X | Fast |\n");
    for (content, unchanged) in [
        ("use X", true),
        ("D2: use X", true),
        ("D2 [stated]: use X", true),
        ("D2 [default] - use X", true),
        ("D2 — use X", true),
        ("Let Em Loose D2 [stated] - use X", true),
        ("let_em_loose D2: use X", true),
        ("Never follow D2; use X", false),
        ("Never follow D2: use X", false),
        ("See D2 use X", false),
        ("D2 use X", false),
        ("other-project D2: use X", false),
        ("D3: use X", false),
        ("D2: stated use X", false),
        ("D2: -use X", false),
    ] {
        let memories = stored(vec![decision(
            "m-d2",
            "let-em-loose",
            content,
            json!({"decision_ref": "D2", "rationale": "Fast"}),
        )]);
        let plan = plan_import(&log, "let-em-loose", "synthetic.md", &memories);
        assert_eq!(plan.unchanged.len(), usize::from(unchanged), "{content}");
        assert_eq!(plan.changed.len(), usize::from(!unchanged), "{content}");
        if !unchanged {
            assert_eq!(plan.changed[0].stored.as_deref(), Some(content));
            let report = render_import(&plan);
            assert!(report.contains(content), "{report}");
            assert!(report.contains("use X"));
        }
    }
}

#[test]
fn code_literals_survive_table_and_section_parsing_and_comparison() {
    for (literal, changed) in [
        ("`**/cache/**`", "`/cache/`"),
        ("`a**b**`", "`ab`"),
        ("`__name__`", "`name`"),
        ("`~~mask~~`", "`mask`"),
        ("`Foo`", "`foo`"),
        ("`x;y`", "`xy`"),
        ("`a  b`", "`a b`"),
        ("``a`**b**``", "``a`b``"),
    ] {
        for section in [false, true] {
            for rationale in [false, true] {
                let title = if rationale {
                    "Use X".into()
                } else {
                    format!("Match {literal}")
                };
                let why = if rationale {
                    format!("Keep {literal}")
                } else {
                    "Fast".into()
                };
                let body = if section {
                    " **Decision.** Keep it.\n"
                } else {
                    ""
                };
                let raw = if section {
                    format!("## D2. {title}\n\n{}**Why.** {why}\n", body.trim_start())
                } else {
                    format!("| # | Decision | Why |\n|---|---|---|\n| D2 | {title} | {why} |\n")
                };
                let log = parse_log(&raw);
                assert_eq!(log.entries[0].title, title);
                assert_eq!(log.entries[0].rationale.as_deref(), Some(why.as_str()));
                let statement = if section {
                    format!("{title} Keep it.")
                } else {
                    title.clone()
                };
                for differs in [false, true] {
                    let content = format!("D2: {statement}");
                    let content = if differs && !rationale {
                        content.replace(literal, changed)
                    } else {
                        content
                    };
                    let stored_why = if differs && rationale {
                        why.replace(literal, changed)
                    } else {
                        why.clone()
                    };
                    let memories = stored(vec![decision(
                        "m-d2",
                        "mill",
                        &content,
                        json!({"decision_ref": "D2", "rationale": stored_why}),
                    )]);
                    let plan = plan_import(&log, "mill", "synthetic.md", &memories);
                    assert_eq!(
                        plan.changed.len(),
                        usize::from(differs),
                        "{raw} vs {content} / {stored_why}"
                    );
                    assert_eq!(plan.unchanged.len(), usize::from(!differs));
                    if differs {
                        assert_eq!(plan.changed[0].stored.as_deref(), Some(content.as_str()));
                        assert_eq!(
                            plan.changed[0].stored_rationale.as_deref(),
                            Some(stored_why.as_str())
                        );
                        let report = render_import(&plan);
                        assert!(report.contains(&title));
                        assert!(report.contains(&why));
                        assert!(report.contains(&content));
                        assert!(report.contains(&stored_why));
                    }
                }
            }
        }
    }
}

#[test]
fn section_decision_body_keeps_code_literals_in_the_report() {
    let log = parse_log(
        "## D2. Cache rule\n\n**Decision.** Match `**/cache/**` using `__key__`.\n**Why.** Fast.\n",
    );
    let original = "D2: Cache rule Match `**/cache/**` using `__key__`.";
    for content in [original, "D2: Cache rule Match `/cache/` using `key`."] {
        let memories = stored(vec![decision(
            "m-d2",
            "mill",
            content,
            json!({"decision_ref": "D2", "rationale": "Fast"}),
        )]);
        let plan = plan_import(&log, "mill", "synthetic.md", &memories);
        assert_eq!(plan.unchanged.len(), usize::from(content == original));
        assert_eq!(plan.changed.len(), usize::from(content != original));
        let report = render_import(&plan);
        assert!(report.contains("Match `**/cache/**` using `__key__`."));
        if content != original {
            assert!(report.contains(content));
        }
    }
}

#[test]
fn escaped_backticks_cannot_hide_literal_delimiter_changes() {
    let log =
        parse_log("| # | Decision | Why |\n|---|---|---|\n| D2 | Match \\``foo\\`` | Fast |\n");
    for (content, unchanged) in [
        (r"D2: Match \`foo\`", false),
        (r"D2: Match \``foo\``", true),
    ] {
        let memories = stored(vec![decision(
            "m-d2",
            "mill",
            content,
            json!({"decision_ref": "D2", "rationale": "Fast"}),
        )]);
        let plan = plan_import(&log, "mill", "synthetic.md", &memories);
        assert_eq!(plan.unchanged.len(), usize::from(unchanged), "{content}");
        assert_eq!(plan.changed.len(), usize::from(!unchanged), "{content}");
    }
}

#[test]
fn multiline_stored_values_stay_inside_their_report_detail() {
    let content =
        "D2: Before\n```\n**literal**  text\n```\n\n## Another heading\n- another item\nend\r\n";
    let why = "Reason\n````\n`a` and \\n are literal\n````\nnext\rline";
    let memories = stored(vec![decision(
        "m-d2",
        "mill",
        content,
        json!({"decision_ref": "D2", "rationale": why}),
    )]);
    let log = parse_log(
        "| # | Decision | Why |\n|---|---|---|\n| D2 | After | Fast |\n| D3 | Later entry | Later reason |\n",
    );
    let plan = plan_import(&log, "mill", "synthetic.md", &memories);
    assert_eq!(plan.changed[0].stored.as_deref(), Some(content));
    assert_eq!(plan.changed[0].stored_rationale.as_deref(), Some(why));
    let report = render_import(&plan);
    let details: Vec<_> = report
        .lines()
        .filter(|line| line.starts_with("  - Stored"))
        .collect();
    assert_eq!(details.len(), 2);
    // The quoted escaped representation preserves line breaks, backslashes,
    // repeated spaces and delimiter runs without adding report structure.
    assert!(details[0].contains(&format!("{content:?}")), "{report}");
    assert!(details[1].contains(&format!("{why:?}")), "{report}");
    for (detail, original) in details.into_iter().zip([content, why]) {
        assert!(detail.contains("escaped string"), "{detail}");
        let rendered = detail.split_once("escaped string ").expect("label").1;
        let fence = rendered.split_once(' ').expect("code delimiter").0;
        let longest = original
            .split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0);
        assert!(fence.len() > longest && fence.chars().all(|c| c == '`'));
        assert!(rendered.ends_with(fence));
    }
    assert!(!report
        .lines()
        .any(|line| line.starts_with("```") || line.starts_with("## Another")));
    assert!(report.contains("\n## Unchanged (0)\n\nNone.\n\n## Needs rationale"));
    assert!(report.contains("D3 (line 4)"));
}

#[test]
fn apply_writes_decision_ref_merged_and_is_idempotent() {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    let memories = Memories::new(&store);
    let rows = [
        decision(
            "m-id",
            "let-em-loose",
            "orch rule",
            json!({"decision_id": "D38", "rationale": "keep me", "stakes": "low"}),
        ),
        decision(
            "m-named",
            "rusty_term",
            "x",
            json!({"decision": "rusty_term D64", "rationale": "x"}),
        ),
        decision(
            "m-open",
            "mill",
            "D7: opens with its ref",
            json!({"rationale": "x"}),
        ),
        decision(
            "m-done",
            "mill",
            "x",
            json!({"decision_ref": "D1", "rationale": "x"}),
        ),
    ];
    for row in &rows {
        memories.insert(row).expect("insert");
    }

    let scoped = apply_normalize(&store, Some("mill")).expect("apply");
    let ids = |rows: &[remind_me_core::decisions_backfill::NormalizeRow]| {
        rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&scoped.updated), ["m-open"], "the project filter holds");

    let applied = apply_normalize(&store, None).expect("apply");
    assert_eq!(ids(&applied.updated), ["m-id", "m-named"]);
    assert!(applied.failed.is_empty(), "{:?}", applied.failed);

    let got = |id: &str| {
        memories
            .all_live()
            .expect("read")
            .into_iter()
            .find(|m| m.id == id)
            .expect("memory")
    };
    let m = got("m-id");
    assert_eq!(m.metadata["decision_ref"], "D38");
    assert_eq!(m.metadata["decision_id"], "D38", "legacy key kept");
    assert_eq!(m.metadata["rationale"], "keep me");
    assert_eq!(m.metadata["stakes"], "low");
    assert_eq!(got("m-named").metadata["decision_ref"], "D64");
    assert_eq!(got("m-open").metadata["decision_ref"], "D7");
    assert_eq!(got("m-done").metadata["decision_ref"], "D1");

    let again = apply_normalize(&store, None).expect("apply");
    assert!(again.updated.is_empty() && again.failed.is_empty());
    assert!(again.plan.add_ref.is_empty());
}

/// Rows edited between planning and writing: the write merges into the
/// newer metadata, a row that stopped qualifying or changed its ref is
/// skipped, and each write records the metadata it replaced as a revision.
#[test]
fn apply_revalidates_each_row_against_the_store_at_write_time() {
    use remind_me_core::db::queries::{delete_memory, update_memory};
    use remind_me_core::models::MemoryUpdateInput;
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    let memories = Memories::new(&store);
    let meta = |r: &str| json!({"decision_id": r, "rationale": "x"});
    for (id, r) in [
        ("m-edit", "D1"),
        ("m-done", "D2"),
        ("m-moved", "D3"),
        ("m-gone", "D4"),
        ("m-ok", "D6"),
    ] {
        memories
            .insert(&decision(id, "mill", "x", meta(r)))
            .expect("insert");
    }
    let plan = plan_normalize(&memories.all_live().expect("read"), Some("mill"));
    assert_eq!(plan.add_ref.len(), 5);

    let set = |id: &str, metadata: serde_json::Value| {
        update_memory(
            &store,
            &MemoryUpdateInput {
                memory_id: id.into(),
                content: None,
                category: None,
                tags: None,
                metadata: Some(metadata),
                sensitive: None,
                clear_superseded: false,
                project: None,
            },
        )
        .expect("update");
    };
    // After planning, before applying:
    set(
        "m-edit",
        json!({"decision_id": "D1", "rationale": "newer", "stakes": "low"}),
    );
    set(
        "m-done",
        json!({"decision_id": "D2", "decision_ref": "D2", "rationale": "x"}),
    );
    set("m-moved", json!({"decision_id": "D30", "rationale": "x"}));
    assert!(delete_memory(&store, "m-gone").expect("delete"));

    let applied = apply_planned(&store, Some("mill"), &plan.add_ref);
    let ids = |rows: &[remind_me_core::decisions_backfill::NormalizeRow]| {
        rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&applied.updated), ["m-edit", "m-ok"]);
    assert_eq!(ids(&applied.skipped), ["m-done", "m-gone", "m-moved"]);
    assert!(
        applied.skipped[2].note.contains("D30"),
        "{}",
        applied.skipped[1].note
    );

    let got = |id: &str| {
        remind_me_core::db::queries::get_memory_by_id(&store, id)
            .expect("read")
            .expect("live")
    };
    let edited = got("m-edit");
    assert_eq!(edited.metadata["decision_ref"], "D1");
    assert_eq!(
        edited.metadata["rationale"], "newer",
        "the newer edit survives"
    );
    assert_eq!(edited.metadata["stakes"], "low");
    assert_eq!(got("m-moved").metadata.get("decision_ref"), None);

    let revisions = remind_me_core::history::history(&store, "m-edit", 10).expect("history");
    let replaced: serde_json::Value = serde_json::from_str(&revisions[0].metadata).expect("json");
    assert_eq!(
        replaced["rationale"], "newer",
        "the revision holds what apply replaced"
    );
    assert_eq!(replaced.get("decision_ref"), None);
}

/// A row whose write is refused is reported and rolled back, and the rows
/// after it are still written.
#[test]
fn apply_isolates_a_failing_row() {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    let memories = Memories::new(&store);
    // An invalid `basis` makes the update path refuse the metadata.
    memories
        .insert(&decision(
            "m-bad",
            "mill",
            "x",
            json!({"decision_id": "D1", "rationale": "x", "basis": "guess"}),
        ))
        .expect("insert");
    memories
        .insert(&decision(
            "m-ok",
            "mill",
            "x",
            json!({"decision_id": "D2", "rationale": "x"}),
        ))
        .expect("insert");
    let applied = apply_normalize(&store, None).expect("apply");
    assert_eq!(
        applied
            .failed
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["m-bad"]
    );
    assert_eq!(
        applied
            .updated
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["m-ok"]
    );
    let bad = remind_me_core::db::queries::get_memory_by_id(&store, "m-bad")
        .expect("read")
        .expect("live");
    assert_eq!(bad.metadata.get("decision_ref"), None);
    assert!(remind_me_core::history::history(&store, "m-bad", 10)
        .expect("history")
        .is_empty());
}
