//! Coverage for `remind_me_export_memories`.

use remind_me_core::db::queries;
use remind_me_core::export::{export_memories, validate_export_path, ExportPathError};
use remind_me_core::{
    Database, EntityInput, ExportFormat, ExportInput, MemoryAddInput, MemoryAnnotation,
};
use rusqlite::Connection;

fn add(
    conn: &Connection,
    content: &str,
    category: &str,
    tags: &[&str],
    entities: &[&str],
) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: category.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: entities
                .iter()
                .map(|n| EntityInput {
                    name: n.to_string(),
                    kind: None,
                    aliases: vec![],
                })
                .collect(),
        },
    )
    .unwrap()
    .id
}

fn export(
    conn: &Connection,
    configure: impl FnOnce(&mut ExportInput),
) -> remind_me_core::ExportResult {
    let mut input = ExportInput {
        include_graph: true,
        ..Default::default()
    };
    configure(&mut input);
    export_memories(conn, &input).unwrap()
}

fn records(result: &remind_me_core::ExportResult) -> Vec<serde_json::Value> {
    serde_json::from_str(result.content.as_ref().unwrap()).unwrap()
}

fn of_type<'a>(records: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    records
        .iter()
        .filter(|r| r.get("record_type").and_then(|v| v.as_str()) == Some(kind))
        .collect()
}

/// A scratch directory inside the default export root (the home directory).
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_export_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_empty_store_exports_an_empty_array() {
    let db = Database::open_in_memory().unwrap();

    let result = export(&db.conn(), |_| {});

    assert_eq!(result.exported, 0);
    assert_eq!(records(&result).len(), 0);
}

#[test]
fn every_memory_column_is_exported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "a memory", "fact", &["tagged"], &[]);

    let result = export(&conn, |i| i.include_graph = false);

    let all = records(&result);
    let memory = &all[0];
    // A backup, not a view — lifecycle fields included.
    for field in [
        "id",
        "content",
        "category",
        "tags",
        "source",
        "metadata",
        "created_at",
        "updated_at",
        "vitality",
        "base_weight",
        "access_count",
        "accessed_at",
        "decay_rate",
        "superseded_by",
        "doc_id",
        "chunk_index",
    ] {
        assert!(memory.get(field).is_some(), "missing {}", field);
    }
    // For importer compatibility.
    assert_eq!(memory["role"], "assistant");
    assert!(
        memory.get("record_type").is_none(),
        "memories carry no discriminator"
    );
}

#[test]
fn jsonl_emits_one_record_per_line() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "first", "fact", &[], &[]);
    add(&conn, "second", "fact", &[], &[]);

    let result = export(&conn, |i| {
        i.format = ExportFormat::Jsonl;
        i.include_graph = false;
    });

    let payload = result.content.unwrap();
    let lines: Vec<&str> = payload.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(parsed.get("id").is_some());
    }
}

#[test]
fn the_category_filter_applies() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "a fact", "fact", &[], &[]);
    add(&conn, "a decision", "decision", &[], &[]);

    let result = export(&conn, |i| {
        i.category = Some("fact".into());
        i.include_graph = false;
    });

    assert_eq!(result.exported, 1);
    assert_eq!(records(&result)[0]["content"], "a fact");
}

#[test]
fn the_tag_filter_is_all_of_not_any_of() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "both tags", "fact", &["alpha", "beta"], &[]);
    add(&conn, "one tag", "fact", &["alpha"], &[]);

    let result = export(&conn, |i| {
        i.tags = Some(vec!["alpha".into(), "beta".into()]);
        i.include_graph = false;
    });

    assert_eq!(result.exported, 1);
    assert_eq!(records(&result)[0]["content"], "both tags");
}

/// Superseded and deleted memories are excluded by default, and available on
/// request (issue #175).
///
/// # This reverses an earlier deliberate position, and is worth reading
///
/// The previous version of this test asserted the opposite — that an export
/// always carries them — arguing: *"Search filters these; a backup must not.
/// Losing superseded history on export would make the backup lossy in a way
/// nothing announces."*
///
/// That concern is real and is **still satisfied**, by `include_deleted: true`
/// rather than by the default. What the old default got wrong is the other
/// half: every exported record is stamped `role: "assistant"` so the importer
/// reads it back as live content, so an export → import round-trip resurrected
/// everything the user had deleted or superseded. A backup that cannot be
/// restored without corrupting the vault is lossy in a louder way than one
/// that omits tombstones.
///
/// The reference splits it exactly here (`exporter.py:163`, `models.py:799`):
/// off by default for moving memories between machines, on for a genuine
/// full-backup or audit export.
#[test]
fn superseded_and_deleted_memories_are_excluded_by_default_and_available_on_request() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let live = add(&conn, "live", "fact", &[], &[]);
    let old = add(&conn, "replaced", "fact", &[], &[]);
    conn.execute(
        "UPDATE memories SET superseded_by = ? WHERE id = ?",
        rusqlite::params![live, old],
    )
    .unwrap();

    let default = export(&conn, |i| i.include_graph = false);
    assert_eq!(
        default.exported, 1,
        "the superseded memory must not ride along by default -- re-importing \
         it would resurrect it as live content"
    );

    // The completeness the old assertion wanted, now opt-in rather than
    // unavoidable.
    let full = export(&conn, |i| {
        i.include_graph = false;
        i.include_deleted = true;
    });
    assert_eq!(
        full.exported, 2,
        "include_deleted must still produce the complete, audit-grade export"
    );
}

#[test]
fn the_graph_is_included_by_default_and_tagged() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "about Tasmania", "fact", &[], &["Tasmania"]);

    let result = export(&conn, |_| {});

    let all = records(&result);
    assert_eq!(of_type(&all, "entity").len(), 1);
    assert_eq!(of_type(&all, "memory_entity").len(), 1);
    assert_eq!(result.entities, Some(1));
    assert_eq!(result.links, Some(1));
    assert_eq!(result.exported, 1, "the count is memories only");
}

#[test]
fn entities_are_emitted_before_the_links_that_reference_them() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "about Tasmania", "fact", &[], &["Tasmania"]);

    let all = records(&export(&conn, |_| {}));

    let entity_at = all
        .iter()
        .position(|r| r.get("record_type").and_then(|v| v.as_str()) == Some("entity"))
        .unwrap();
    let link_at = all
        .iter()
        .position(|r| r.get("record_type").and_then(|v| v.as_str()) == Some("memory_entity"))
        .unwrap();
    // A sequential restore has to be able to verify a link's endpoints exist.
    assert!(entity_at < link_at);
}

#[test]
fn the_graph_can_be_excluded() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "about Tasmania", "fact", &[], &["Tasmania"]);

    let result = export(&conn, |i| i.include_graph = false);

    assert!(records(&result)
        .iter()
        .all(|r| r.get("record_type").is_none()));
    assert_eq!(result.entities, None);
}

#[test]
fn relations_are_exported() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(
        &conn,
        "Bailey lives in Hobart",
        "fact",
        &[],
        &["Bailey", "Hobart"],
    );
    queries::annotate_memories(
        &conn,
        &remind_me_core::AnnotateInput {
            annotations: vec![MemoryAnnotation {
                memory_id: id,
                subject: Some("Bailey".into()),
                predicate: Some("lives_in".into()),
                object: Some("Hobart".into()),
                entities: vec![],
            }],
        },
    )
    .unwrap();

    let result = export(&conn, |_| {});

    assert_eq!(result.relations, Some(1));
    let all = records(&result);
    assert_eq!(of_type(&all, "entity_relation")[0]["relation"], "lives_in");
}

#[test]
fn a_filtered_export_scopes_the_graph_to_what_it_reaches() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "kept", "fact", &[], &["Tasmania"]);
    add(&conn, "dropped", "decision", &[], &["Fiji"]);

    let result = export(&conn, |i| i.category = Some("fact".into()));

    let all = records(&result);
    let entities = of_type(&all, "entity");
    // Only the entity the exported memory references — an unreferenced one
    // would be noise, and a link to a memory outside the export would dangle.
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0]["name"], "Tasmania");
    assert_eq!(of_type(&all, "memory_entity").len(), 1);
}

#[test]
fn a_filtered_export_drops_relations_with_an_endpoint_outside_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let kept = add(&conn, "kept", "fact", &[], &["Bailey"]);
    add(&conn, "dropped", "decision", &[], &["Hobart"]);
    queries::annotate_memories(
        &conn,
        &remind_me_core::AnnotateInput {
            annotations: vec![MemoryAnnotation {
                memory_id: kept,
                subject: Some("Bailey".into()),
                predicate: Some("lives_in".into()),
                object: Some("Hobart".into()),
                entities: vec![],
            }],
        },
    )
    .unwrap();
    // Unfiltered, the edge is exported.
    assert_eq!(export(&conn, |_| {}).relations, Some(1));

    let filtered = export(&conn, |i| i.category = Some("fact".into()));

    // Hobart is not reachable from the exported set, so the edge would dangle
    // on restore. Both endpoints have to be in scope.
    assert_eq!(filtered.relations, Some(0));
}

// --- destination validation --------------------------------------------------

#[test]
fn an_export_writes_to_a_file_and_reports_its_size() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "a memory", "fact", &[], &[]);
    let dir = scratch("write");
    let path = dir.join("export.json");

    let result = export(&conn, |i| {
        i.file_path = Some(path.display().to_string());
        i.include_graph = false;
    });

    assert!(result.content.is_none(), "a file export is not also inline");
    assert_eq!(
        result.file.as_deref(),
        Some(path.display().to_string().as_str())
    );
    let written = std::fs::read(&path).unwrap();
    assert_eq!(result.bytes, Some(written.len()));
    let parsed: Vec<serde_json::Value> = serde_json::from_slice(&written).unwrap();
    assert_eq!(parsed.len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_path_outside_the_roots_is_rejected() {
    // Containment is checked before anything touches the filesystem, so this
    // must fail the same way whether or not the path exists — otherwise the
    // export tool answers "does this file exist?" for any path on the machine.
    assert!(matches!(
        validate_export_path("/etc/passwd"),
        Err(ExportPathError::OutsideRoots(_))
    ));
    assert!(matches!(
        validate_export_path("/etc/definitely-not-here-12345"),
        Err(ExportPathError::OutsideRoots(_))
    ));
}

#[test]
fn a_traversal_out_of_the_roots_is_rejected() {
    let home = remind_me_core::import_paths::home_dir_var().unwrap();

    // Resolving before the containment test is what stops this.
    assert!(matches!(
        validate_export_path(&format!("{}/../../etc/passwd", home)),
        Err(ExportPathError::OutsideRoots(_))
    ));
}

#[test]
fn a_directory_destination_is_rejected() {
    let dir = scratch("isdir");

    assert!(matches!(
        validate_export_path(&dir.display().to_string()),
        Err(ExportPathError::IsADirectory(_))
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_parent_directory_is_rejected() {
    let home = remind_me_core::import_paths::home_dir_var().unwrap();

    assert!(matches!(
        validate_export_path(&format!("{}/no_such_dir_98765/export.json", home)),
        Err(ExportPathError::NoParentDirectory(_))
    ));
}

#[test]
fn a_path_inside_the_roots_is_accepted() {
    let dir = scratch("ok");
    let path = dir.join("export.json");

    assert!(validate_export_path(&path.display().to_string()).is_ok());

    std::fs::remove_dir_all(&dir).unwrap();
}
