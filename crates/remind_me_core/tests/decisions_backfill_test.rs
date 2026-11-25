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
        applied.skipped[2].note
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

/// A project claim row is never written by the generic apply: it is
/// skipped with the claim refusal, left as it was, and the rest still go.
#[test]
fn apply_skips_claim_rows() {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    let memories = Memories::new(&store);
    let mut claim = decision(
        "m-claim",
        "mill",
        "x",
        json!({"decision_id": "D1", "rationale": "x"}),
    );
    claim.subject = Some("mill".into());
    claim.predicate = Some("project.stage".into());
    claim.object = Some("built".into());
    memories.insert(&claim).expect("insert");
    memories
        .insert(&decision(
            "m-ok",
            "mill",
            "x",
            json!({"decision_id": "D2", "rationale": "x"}),
        ))
        .expect("insert");

    let applied = apply_normalize(&store, None).expect("apply");
    let ids = |rows: &[remind_me_core::decisions_backfill::NormalizeRow]| {
        rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&applied.plan.add_ref), ["m-claim", "m-ok"]);
    assert_eq!(ids(&applied.skipped), ["m-claim"]);
    assert!(
        applied.skipped[0].note.contains("claim"),
        "{}",
        applied.skipped[0].note
    );
    assert_eq!(ids(&applied.updated), ["m-ok"]);
    let claim = remind_me_core::db::queries::get_memory_by_id(&store, "m-claim")
        .expect("read")
        .expect("live");
    assert_eq!(claim.metadata.get("decision_ref"), None);
    assert!(remind_me_core::history::history(&store, "m-claim", 10)
        .expect("history")
        .is_empty());
}

// -- ref options and import apply (pack D69, D70, D74) -------------------------

use remind_me_core::decisions_backfill::{
    apply_import, parse_log_with, render_import_applied, RefOptions, IMPORT_SOURCE,
};

const DATED: &str = "\
| Date | Decision | Why | Owner |
|---|---|---|---|
| 2026-10-21 | Build fresh | Structural gap | Owner |
| 2026-10-21 | ~~tokio is the runtime~~ | Superseded | Owner |
| 2026-10-21 | One VT engine | Reuse | Owner |
| 2026-10-22 | Crate prefix rusty_mux | Confirmed | Owner |
| soon | Undated row | None | Owner |
";

#[test]
fn a_dated_table_gets_generated_refs_counted_per_date_in_file_order() {
    let options = RefOptions {
        generate: Some("mux".into()),
        ..RefOptions::default()
    };
    let log = parse_log_with(DATED, &options);
    let refs: Vec<(&str, bool)> = log
        .entries
        .iter()
        .map(|e| (e.decision_ref.as_str(), e.struck))
        .collect();
    assert_eq!(
        refs,
        [
            ("mux-2026-10-21-1", false),
            ("mux-2026-10-21-2", true),
            ("mux-2026-10-21-3", false),
            ("mux-2026-10-22-1", false),
        ],
        "a struck row keeps its number, so later refs never shift"
    );
    assert_eq!(log.unparsed.len(), 1, "a row with no date is not guessed");

    let plain = parse_log(DATED);
    assert!(plain.entries.is_empty(), "no refs are made up unless asked");
    assert_eq!(plain.unparsed.len(), 5);
}

const GUI: &str = "\
| # | Decision | Why |
|---|---|---|
| D1 | Dark theme first | Taste |
| D2 | Supersedes D1. Light theme first | Changed mind |
| D2 | Tabs on top | Habit |
";

#[test]
fn a_prefix_qualifies_refs_and_links_and_a_repeat_gets_a_letter() {
    let options = RefOptions {
        prefix: Some("gui-design".into()),
        rename_duplicates: true,
        ..RefOptions::default()
    };
    let log = parse_log_with(GUI, &options);
    let refs: Vec<&str> = log
        .entries
        .iter()
        .map(|e| e.decision_ref.as_str())
        .collect();
    assert_eq!(refs, ["gui-design D1", "gui-design D2", "gui-design D2b"]);
    assert_eq!(log.renamed.len(), 1);
    assert_eq!(log.renamed[0].from, "gui-design D2");
    assert_eq!(log.renamed[0].to, "gui-design D2b");
    assert_eq!(log.renamed[0].line, 5);

    let plan = plan_import(&log, "rusty_term", "gui.md", &[]);
    assert_eq!(plan.new.len(), 3);
    assert!(plan.conflicts.is_empty());
    assert_eq!(plan.supersedes.len(), 1);
    assert_eq!(plan.supersedes[0].supersedes, "gui-design D1");
    assert!(render_import(&plan).contains("gui-design D2 -> gui-design D2b"));

    let strict = parse_log_with(
        GUI,
        &RefOptions {
            prefix: Some("gui-design".into()),
            ..RefOptions::default()
        },
    );
    let plan = plan_import(&strict, "rusty_term", "gui.md", &[]);
    assert_eq!(
        plan.conflicts.len(),
        2,
        "without the flag a repeat imports neither"
    );
}

const SMALL: &str = "\
| # | Decision | Why | Tag |
|---|---|---|---|
| D1 | Use the engine store | Measured | [stated] |
| D2 | Supersedes D1. Use SQLite | Benchmark | [stated] |
| D3 | Confirms D2 | Checked again | [default] |
| D4 | No rationale |  | [default] |
";

#[test]
fn apply_adds_new_entries_links_them_and_a_second_run_adds_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let log = parse_log(SMALL);

    let applied = apply_import(&store, &log, "rusty-remind-me", "docs/DECISIONS.md").unwrap();
    let added: Vec<&str> = applied
        .added
        .iter()
        .map(|r| r.decision_ref.as_str())
        .collect();
    assert_eq!(added, ["D1", "D2", "D3"]);
    assert!(applied.failed.is_empty() && applied.skipped.is_empty());
    assert_eq!(applied.linked.len(), 1);
    assert!(applied.unlinked.is_empty());
    assert_eq!(
        applied.plan.needs_rationale.len(),
        1,
        "D4 is left for a person"
    );
    assert!(render_import_applied(&applied).contains("Added 3 decisions"));

    let id = |r: &str| {
        applied
            .added
            .iter()
            .find(|a| a.decision_ref == r)
            .and_then(|a| a.memory_id.clone())
            .unwrap()
    };
    let memories = Memories::new(&store);
    let d1 = memories.get_live(&id("D1")).unwrap().unwrap();
    let d2 = memories.get_live(&id("D2")).unwrap().unwrap();
    let d3 = memories.get_live(&id("D3")).unwrap().unwrap();
    assert_eq!(d1.superseded_by.as_deref(), Some(id("D2").as_str()));
    assert_eq!(d2.memory_type.as_deref(), Some("decision"));
    assert_eq!(d2.category, "decision");
    assert_eq!(d2.source, IMPORT_SOURCE);
    assert_eq!(d2.project.as_deref(), Some("rusty-remind-me"));
    assert_eq!(d2.content, "Supersedes D1. Use SQLite");
    assert_eq!(d2.metadata["decision_ref"], "D2");
    assert_eq!(d2.metadata["rationale"], "Benchmark");
    assert_eq!(d2.metadata["tag"], "[stated]");
    assert_eq!(
        d2.metadata["imported"],
        json!({ "source": "docs/DECISIONS.md", "line": 4 })
    );
    assert!(
        d2.metadata.get("review").is_none(),
        "an imported decision gets no review date (D74)"
    );
    assert_eq!(d3.metadata["confirms"], json!(["D2"]));

    let again = apply_import(&store, &log, "rusty-remind-me", "docs/DECISIONS.md").unwrap();
    assert!(again.added.is_empty() && again.linked.is_empty());
    assert_eq!(
        again.plan.unchanged.len(),
        3,
        "the retired D1 counts as stored, never as new again"
    );
}

#[test]
fn apply_never_overwrites_a_stored_decision_and_leaves_an_unknown_target_unlinked() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let mut stored = NewMemory::new("mem_d1", "Something else entirely", "2026-11-01T00:00:00Z");
    stored.memory_type = "decision".into();
    stored.project = Some("rusty-remind-me".into());
    stored.metadata = json!({ "decision_ref": "D1", "rationale": "older" });
    Memories::new(&store).insert(&stored).unwrap();
    let log = parse_log(
        "| # | Decision | Why |\n|---|---|---|\n| D1 | Use the engine store | Measured |\n| D9 | Supersedes D8. New | Why |\n",
    );

    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(applied.plan.changed.len(), 1);
    let kept = Memories::new(&store).get_live("mem_d1").unwrap().unwrap();
    assert_eq!(kept.content, "Something else entirely");
    assert_eq!(applied.added.len(), 1, "D9 still imports");
    assert_eq!(applied.unlinked.len(), 1, "D8 is not stored: not guessed");
}

/// Each added row's ref, in the order the run wrote them.
fn added_refs(applied: &remind_me_core::decisions_backfill::ImportApplied) -> Vec<&str> {
    applied
        .added
        .iter()
        .map(|r| r.decision_ref.as_str())
        .collect()
}

/// The stored decision `applied` added for `reference`.
fn added_memory(
    store: &remind_me_core::db::Store<'_>,
    applied: &remind_me_core::decisions_backfill::ImportApplied,
    reference: &str,
) -> Memory {
    let id = applied
        .added
        .iter()
        .find(|r| r.decision_ref == reference)
        .and_then(|r| r.memory_id.clone())
        .expect("added");
    Memories::new(store)
        .get_live(&id)
        .expect("read")
        .expect("stored")
}

#[test]
fn apply_writes_a_target_before_the_entry_that_supersedes_it_in_any_file_order() {
    let reversed = "\
| # | Decision | Why |
|---|---|---|
| D2 | Supersedes D1. Use SQLite | Benchmark |
| D1 | Use the engine store | Measured |
";
    let mixed = "\
| # | Decision | Why |
|---|---|---|
| D2 | Supersedes D3. Use SQLite | Benchmark |

## D3. Use the engine store

**Decision.** Keep it.
**Why.** Measured.
";
    for (text, target) in [(reversed, "D1"), (mixed, "D3")] {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let applied = apply_import(&store, &parse_log(text), "rusty-remind-me", "d.md").unwrap();
        assert_eq!(added_refs(&applied), [target, "D2"], "{target}");
        assert_eq!(applied.linked.len(), 1, "{target}");
        assert!(applied.unlinked.is_empty() && applied.skipped.is_empty());
        let older = added_memory(&store, &applied, target);
        let newer = added_memory(&store, &applied, "D2");
        assert_eq!(older.superseded_by.as_deref(), Some(newer.id.as_str()));
        assert!(newer.superseded_by.is_none());
    }
}

#[test]
fn apply_refuses_a_supersedes_cycle_before_writing_anything() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let log = parse_log(
        "| # | Decision | Why |\n|---|---|---|\n| D1 | Supersedes D2. A | x |\n| D2 | Supersedes D1. B | y |\n",
    );
    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert!(applied.added.is_empty());
    assert_eq!(applied.skipped.len(), 2);
    assert!(applied.skipped[0].note.contains("cycle"));
    assert!(Memories::new(&store).all_live().unwrap().is_empty());
}

#[test]
fn apply_holds_back_an_entry_whose_target_in_the_file_was_not_written() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // D1 has no rationale, so it is left for a person; D2 must not go in
    // without its link and leave both live once D1 is imported later.
    let log = parse_log(
        "| # | Decision | Why |\n|---|---|---|\n| D1 | Old |  |\n| D2 | Supersedes D1. New | Why |\n",
    );
    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert!(applied.added.is_empty());
    assert_eq!(applied.skipped.len(), 1);
    assert_eq!(applied.skipped[0].decision_ref, "D2");
    assert!(applied.skipped[0].note.contains("D1"));
}

#[test]
fn apply_leaves_a_target_whose_refs_disagree_unlinked_and_live() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let mut odd = NewMemory::new("mem_odd", "An older call", "2026-11-01T00:00:00Z");
    odd.memory_type = "decision".into();
    odd.project = Some("rusty-remind-me".into());
    odd.metadata = json!({ "decision_ref": "D1", "decision_id": "D2", "rationale": "x" });
    Memories::new(&store).insert(&odd).unwrap();
    let log =
        parse_log("| # | Decision | Why |\n|---|---|---|\n| D5 | Supersedes D1. New | Why |\n");

    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(added_refs(&applied), ["D5"]);
    assert_eq!(applied.unlinked.len(), 1);
    assert!(applied.linked.is_empty());
    let kept = Memories::new(&store).get_live("mem_odd").unwrap().unwrap();
    assert!(
        kept.superseded_by.is_none(),
        "an untrusted identity is never retired"
    );
}

#[test]
fn apply_writes_nothing_for_a_row_with_a_plain_and_an_ambiguous_link() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let first = parse_log("| # | Decision | Why |\n|---|---|---|\n| D1 | Old | x |\n");
    apply_import(&store, &first, "rusty-remind-me", "d.md").unwrap();
    let log = parse_log(
        "| # | Decision | Why |\n|---|---|---|\n| D1 | Old | x |\n| D5 | Supersedes D1; not supersedes D2. New | Why |\n",
    );

    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(applied.plan.ambiguous.len(), 1);
    assert!(applied.added.is_empty() && applied.linked.is_empty());
    let live = Memories::new(&store).all_live().unwrap();
    assert_eq!(live.len(), 1);
    assert!(live[0].superseded_by.is_none(), "D1 is not retired");
}

#[test]
fn apply_retires_a_target_named_twice_by_one_entry_once() {
    let same_sentence = "\
| # | Decision | Why |
|---|---|---|
| D1 | Old policy | x |
| D2 | Supersedes D1. New policy supersedes D1. | y |
";
    let title_and_body = "\
| # | Decision | Why |
|---|---|---|
| D1 | Old policy | x |

## D2. Supersedes D1.

**Decision.** This supersedes D1.
**Why.** y
";
    for text in [same_sentence, title_and_body] {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let applied = apply_import(&store, &parse_log(text), "rusty-remind-me", "d.md").unwrap();
        assert!(applied.failed.is_empty(), "{:?}", applied.failed);
        assert_eq!(added_refs(&applied), ["D1", "D2"]);
        assert_eq!(applied.linked.len(), 1, "the link is reported once");
        let older = added_memory(&store, &applied, "D1");
        let newer = added_memory(&store, &applied, "D2");
        assert_eq!(older.superseded_by.as_deref(), Some(newer.id.as_str()));
    }
}

const LINK_FIRST: &str = "\
| # | Decision | Why |
|---|---|---|
| D1 | Old policy | x |
| D2 | Supersedes D1: New policy | y |
";
const LINK_THIRD: &str = "| D3 | Supersedes D2. Newer policy | z |\n";

/// D1 and D2 retired, D3 live: `Supersedes D1:` opening D2 was read as a
/// link, never as D2 defining D1.
fn assert_chain_retired(store: &remind_me_core::db::Store<'_>) {
    let live = Memories::new(store).all_live().unwrap();
    let retired = |r: &str| {
        live.iter()
            .find(|m| m.metadata["decision_ref"] == r)
            .map(|m| m.superseded_by.is_some())
            .expect(r)
    };
    assert!(retired("D1") && retired("D2"), "D1 and D2 are retired");
    assert!(!retired("D3"), "D3 is live");
}

#[test]
fn a_link_clause_opening_an_entry_is_not_its_identity_in_one_import() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let log = parse_log(&format!("{LINK_FIRST}{LINK_THIRD}"));
    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(added_refs(&applied), ["D1", "D2", "D3"]);
    assert_eq!(applied.linked.len(), 2);
    assert!(applied.unlinked.is_empty(), "{:?}", applied.unlinked);
    assert_chain_retired(&store);
}

#[test]
fn a_link_clause_opening_an_entry_is_not_its_identity_across_two_imports() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    apply_import(&store, &parse_log(LINK_FIRST), "rusty-remind-me", "d.md").unwrap();
    let log = parse_log(&format!("{LINK_FIRST}{LINK_THIRD}"));
    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(added_refs(&applied), ["D3"]);
    assert_eq!(applied.linked.len(), 1, "D3 -> D2 is linked");
    assert!(applied.unlinked.is_empty(), "{:?}", applied.unlinked);
    assert_chain_retired(&store);
}

/// The exported log's rows as (ref, date) pairs, in log order.
fn exported_dates(store: &remind_me_core::db::Store<'_>) -> Vec<(String, String)> {
    use remind_me_core::decisions_log::{export, DecisionsQuery};
    let log = export(
        store,
        &DecisionsQuery {
            project: "rusty-remind-me".into(),
            include_superseded: true,
        },
    )
    .unwrap();
    log.markdown
        .lines()
        .filter(|l| l.starts_with("| D"))
        .skip(1) // the header row
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            (cells[1].to_string(), cells[2].to_string())
        })
        .collect()
}

#[test]
fn an_imported_entry_exports_with_its_logged_date() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let log = parse_log("| # | Date | Decision | Why |\n|---|---|---|---|\n| D1 | 2026-10-21 | Build fresh | Gap |\n");
    apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(exported_dates(&store), [("D1".into(), "2026-10-21".into())]);
}

#[test]
fn imported_entries_export_in_logged_date_order() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let log = parse_log(
        "| # | Date | Decision | Why |\n|---|---|---|---|\n| D1 | 2026-10-21 | Later | x |\n| D2 | 2026-09-02 | Earlier | y |\n",
    );
    apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert_eq!(
        exported_dates(&store),
        [
            ("D2".into(), "2026-09-02".into()),
            ("D1".into(), "2026-10-21".into())
        ]
    );
}

#[test]
fn an_undated_import_exports_its_created_day() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let log = parse_log("| # | Decision | Why |\n|---|---|---|\n| D1 | Build fresh | Gap |\n");
    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    let created = added_memory(&store, &applied, "D1").created_at;
    assert_eq!(
        exported_dates(&store),
        [("D1".into(), created[..10].to_string())]
    );
}

#[test]
fn a_malformed_date_is_reported_not_stored() {
    let log = parse_log(
        "| # | Date | Decision | Why |\n|---|---|---|---|\n| D1 | Oct 21 | Build fresh | Gap |\n| D2 | 2026-02-30 | Other | x |\n",
    );
    assert!(log.entries.is_empty(), "{:?}", log.entries);
    assert_eq!(log.unparsed.len(), 2);
    assert!(log.unparsed.iter().all(|u| u.reason.contains("YYYY-MM-DD")));

    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let applied = apply_import(&store, &log, "rusty-remind-me", "d.md").unwrap();
    assert!(applied.added.is_empty());
    assert!(Memories::new(&store).all_live().unwrap().is_empty());
}

#[test]
fn an_entry_superseding_a_row_rejected_for_its_date_waits_for_it() {
    let log_with = |date: &str| {
        parse_log(&format!(
            "| # | Date | Decision | Why |\n|---|---|---|---|\n| D1 | {date} | Old | x |\n| D2 | 2026-10-22 | Supersedes D1. New | y |\n"
        ))
    };
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let bad = log_with("Oct 21");
    assert_eq!(bad.unparsed.len(), 1, "D1 is reported");
    let applied = apply_import(&store, &bad, "rusty-remind-me", "d.md").unwrap();
    assert!(applied.added.is_empty());
    assert_eq!(applied.skipped.len(), 1);
    assert_eq!(applied.skipped[0].decision_ref, "D2");
    assert!(
        applied.skipped[0].note.contains("D1") && applied.skipped[0].note.contains("rejected"),
        "{}",
        applied.skipped[0].note
    );
    assert!(Memories::new(&store).all_live().unwrap().is_empty());

    let fixed = apply_import(&store, &log_with("2026-10-21"), "rusty-remind-me", "d.md").unwrap();
    assert_eq!(added_refs(&fixed), ["D1", "D2"]);
    assert_eq!(fixed.linked.len(), 1);
    let live: Vec<_> = Memories::new(&store)
        .all_live()
        .unwrap()
        .into_iter()
        .filter(|m| m.superseded_by.is_none())
        .map(|m| m.metadata["decision_ref"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(live, ["D2"], "only D2 is live");
}

fn mux_log(first_text: &str) -> String {
    format!(
        "| Date | Decision | Why |\n|---|---|---|\n| 2026-10-21 | {first_text} | a |\n| 2026-10-21 | Decision B | b |\n| 2026-10-22 | Other day | c |\n"
    )
}

fn mux_options() -> RefOptions {
    RefOptions {
        generate: Some("mux".into()),
        ..RefOptions::default()
    }
}

#[test]
fn a_rejected_generated_row_keeps_its_place_in_the_date_sequence() {
    let log = parse_log_with(&mux_log(""), &mux_options());
    let refs: Vec<&str> = log
        .entries
        .iter()
        .map(|e| e.decision_ref.as_str())
        .collect();
    assert_eq!(refs, ["mux-2026-10-21-2", "mux-2026-10-22-1"]);
    let rejected: Vec<&str> = log
        .rejected_refs
        .iter()
        .map(|r| r.decision_ref.as_str())
        .collect();
    assert_eq!(rejected, ["mux-2026-10-21-1"]);
}

#[test]
fn mending_a_rejected_generated_row_imports_it_and_leaves_the_rest_unchanged() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let first = parse_log_with(&mux_log(""), &mux_options());
    let applied = apply_import(&store, &first, "rusty_mux", "mux.md").unwrap();
    assert_eq!(
        added_refs(&applied),
        ["mux-2026-10-21-2", "mux-2026-10-22-1"]
    );

    let mended = parse_log_with(&mux_log("Decision A"), &mux_options());
    let again = apply_import(&store, &mended, "rusty_mux", "mux.md").unwrap();
    assert_eq!(added_refs(&again), ["mux-2026-10-21-1"]);
    assert_eq!(
        again.plan.unchanged.len(),
        2,
        "B and the other day are unchanged"
    );
    assert!(again.plan.changed.is_empty());
    assert_eq!(
        Memories::new(&store).all_live().unwrap().len(),
        3,
        "no second copy of B"
    );
}
