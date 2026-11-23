//! The R3 store benchmark (#1618) on the backends in this crate. The
//! workloads are in `harness.rs`; candidates outside the workspace (Attest,
//! D45) include that file and add themselves.

use remind_me_core::db::backend::Backend;
use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::sqlite::SqliteTables;
use std::path::Path;

#[path = "harness.rs"]
mod harness;

use harness::{backup_package, Candidate, Result};

fn engine(dir: &Path) -> Result<Box<dyn Backend>> {
    Ok(Box::new(EngineTables::open(dir)?))
}

fn sqlite(dir: &Path) -> Result<Box<dyn Backend>> {
    Ok(Box::new(SqliteTables::open(dir)?))
}

const ENGINE: Candidate = Candidate {
    name: "engine",
    tables: engine,
    restore: engine,
    package: backup_package,
    verify: harness::verify_engine,
};

const SQLITE: Candidate = Candidate {
    name: "sqlite",
    tables: sqlite,
    restore: sqlite,
    package: backup_package,
    verify: harness::verify_sqlite,
};

fn main() -> Result<()> {
    harness::run(&[ENGINE, SQLITE])
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness::{measure, Dataset, Isolation, Mode};

    /// The smallest store has no editable rows (its one memory is a
    /// dialog): updates and deletes report zero samples and say why, and
    /// the run still completes.
    #[test]
    fn the_minimum_size_reports_skipped_edits() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_min_{}", std::process::id()));
        let report = measure(
            SQLITE,
            Mode::Bench,
            &Dataset::Synthetic(1),
            &root,
            Isolation::InProcess,
        )
        .unwrap();
        for metric in ["update", "delete"] {
            assert_eq!(report[metric]["count"], 0, "{metric}");
            assert!(report[metric]["skipped"].is_string(), "{metric}");
        }
        assert_eq!(report["restore"]["matched"], true);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_zero_size_is_refused_before_any_directory_is_made() {
        let root = std::env::temp_dir().join(format!("store_bench_zero_{}", std::process::id()));
        assert!(measure(
            ENGINE,
            Mode::Bench,
            &Dataset::Synthetic(0),
            &root,
            Isolation::InProcess
        )
        .is_err());
        assert!(!root.exists());
    }

    /// Memories in the D1 fixture: enough for 500 distinct queries.
    const FIXTURE: usize = 520;
    /// The memories the fixture's project claim adds (stage and status).
    #[cfg(unix)]
    const CLAIMS: usize = 2;

    /// A small engine store backed up as `manual-*.engine`: D1's source.
    fn small_backup(root: &Path) -> std::path::PathBuf {
        use remind_me_core::db::derived::Origin;
        use remind_me_core::db::entities::{Entities, RelationRow};
        use remind_me_core::db::queries;
        use remind_me_core::db::references::{NewReference, References};
        use remind_me_core::db::vectors::Vectors;
        use remind_me_core::entity;
        use remind_me_core::models::{EntityInput, MemoryAddInput};
        use remind_me_core::Database;

        let dir = root.join("source");
        std::fs::create_dir_all(&dir).unwrap();
        let tables = EngineTables::open(&dir.join("store")).unwrap();
        let db = Database::open_backend(Box::new(tables), Some(dir.join("memory.db"))).unwrap();
        let store = db.store();
        let mut ids = Vec::new();
        for i in 0..FIXTURE {
            let input = MemoryAddInput {
                content: format!(
                    "memory number {i} about rust storage engines and {}",
                    "words ".repeat(i % 40)
                ),
                category: if i % 3 == 0 { "dialog" } else { "fact" }.into(),
                ..Default::default()
            };
            let id = queries::add_memory(&store, input).unwrap().id;
            let v: Vec<u8> = (0..4 * 8).map(|b| (b + i % 200) as u8).collect();
            Vectors::new(&store).put(&id, 0, &v).unwrap();
            ids.push(id);
        }
        let upsert = |name: &str| {
            let input = EntityInput {
                name: name.into(),
                kind: None,
                aliases: vec![],
            };
            entity::upsert_entity(&store, &input).unwrap().id
        };
        let (a, b) = (upsert("a"), upsert("b"));
        Entities::new(&store)
            .link(&ids[1], &a, "2026-11-22", Origin::Sync)
            .unwrap();
        let relation = RelationRow {
            id: "r1",
            subject_entity_id: &a,
            relation: "knows",
            object_entity_id: &b,
            created_at: "2026-11-22",
            updated_at: "2026-11-22",
            node_id: Some("node-x"),
        };
        Entities::new(&store)
            .insert_relation_or_ignore(&relation, Origin::Sync)
            .unwrap();
        References::new(&store)
            .insert(&NewReference::new(
                &ids[2],
                "url",
                "https://example.com",
                "2026-11-22",
            ))
            .unwrap();
        // A project claim: append-only, so the edit samples must skip it.
        use remind_me_core::project::{self, ClaimValue, SetInput, Stage, Status};
        project::set(
            &store,
            &SetInput {
                project: "a".into(),
                claims: vec![
                    ClaimValue::Stage(Stage::Idea),
                    ClaimValue::Status(Status::Active),
                ],
                evidence: vec![ids[3].clone()],
                because: None,
                confirmed_by: "human".into(),
                proposal: None,
            },
        )
        .unwrap();
        remind_me_core::backup::create_backup(&store, "manual")
            .unwrap()
            .path
            .into()
    }

    /// The variable carrying a crash child's arguments to this test runner.
    #[cfg(unix)]
    const CRASH_CHILD: &str = "STORE_BENCH_CRASH_CHILD";

    /// A crash child in this test runner, which has no `crash-child` mode:
    /// the runner re-run on `crash_child_entry` alone.
    #[cfg(unix)]
    fn test_child(args: &[String]) -> Result<std::process::Command> {
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args(["tests::crash_child_entry", "--exact", "--nocapture"])
            .args(["--test-threads=1", "-q"])
            .env(CRASH_CHILD, serde_json::to_string(args)?);
        Ok(command)
    }

    /// The crash child when the runner is started as one; else nothing.
    #[cfg(unix)]
    #[test]
    fn crash_child_entry() {
        let Ok(args) = std::env::var(CRASH_CHILD) else {
            return;
        };
        let args: Vec<String> = serde_json::from_str(&args).unwrap();
        harness::crash::child_from(&[ENGINE, SQLITE], &args).unwrap();
    }

    /// G6: writers killed for real, ten rounds of each stream on a copy of
    /// a small D1. On both candidates they leave only whole writes and every
    /// check passes, the integrity check included. The engine passes since
    /// its transactions cover `memory_revisions` (Q22, D62).
    #[cfg(unix)]
    #[test]
    fn killed_writers_leave_whole_writes() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_crash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        let set = format!("d1:{}", backup.display());
        for candidate in [ENGINE, SQLITE] {
            let report =
                harness::crash::run(candidate, &set, &root.join("runs"), 10, test_child).unwrap();
            let report = serde_json::Value::Object(report);
            assert!(report.get("aborted").is_none(), "{report}");
            assert_eq!(report["passed"], true, "{report}");
            let rounds = report["rounds"].as_array().unwrap();
            assert_eq!(rounds.len(), 20, "{report}");
            for stream in ["writes", "pages"] {
                let acked: u64 = rounds
                    .iter()
                    .filter(|r| r["stream"] == stream)
                    .map(|r| r["acked"].as_u64().unwrap())
                    .sum();
                assert!(acked > 0, "{stream}: {report}");
            }
            assert!(
                report["final"]["check"]["integrity"].is_object(),
                "{report}"
            );
            assert!(report["memories_tracked"].as_u64().unwrap() > 0, "{report}");
            for stream in ["writes", "pages"] {
                let kills = &report["kills"][stream];
                let fell: u64 = [
                    "between_writes",
                    "inside_a_write_not_landed",
                    "inside_a_write_landed",
                ]
                .iter()
                .map(|k| kills[*k].as_u64().unwrap())
                .sum();
                assert_eq!(kills["total"], 10, "{report}");
                assert_eq!(fell, 10, "every kill falls somewhere: {report}");
            }
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// D1 loads every row kind through the ports into each candidate, and
    /// the run completes with an isolated restore and a cold open.
    #[cfg(unix)] // real datasets need a verifiable private root
    #[test]
    fn d1_loads_the_backup_into_each_candidate() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_d1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        for candidate in [ENGINE, SQLITE] {
            let dataset = Dataset::D1(backup.clone());
            let report = measure(
                candidate,
                Mode::Bench,
                &dataset,
                &root.join("runs"),
                Isolation::InProcess,
            )
            .unwrap();
            let run = Path::new(report["run_dir"].as_str().unwrap());
            let owned = run.parent().unwrap();
            assert!(owned.join("run.package").is_dir(), "package inside");
            assert!(owned.join("run.restored").is_dir(), "restore inside");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(owned).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "{}", candidate.name);
            }
            let loaded = &report["loaded"];
            for (kind, n) in [
                ("memories", FIXTURE + CLAIMS),
                ("vectors", FIXTURE),
                ("entities", 2),
                ("links", 1),
                ("relations", 1),
                ("references", 1),
            ] {
                assert_eq!(loaded[kind], n, "{} {kind}", candidate.name);
            }
            assert_eq!(report["restore"]["matched"], true, "{}", candidate.name);
            assert_eq!(report["parity"]["matched"], true, "{}", candidate.name);
            assert!(report["parity"]["against"]
                .as_str()
                .unwrap()
                .contains("not the untouched backup"));
            // The claim (and the dialogs) stay loaded but out of the edits.
            assert_eq!(
                report["edit_pool"]["append_only"],
                FIXTURE.div_ceil(3) + CLAIMS,
                "every dialog and both claims"
            );
            for metric in ["update", "delete"] {
                assert!(report[metric]["count"].as_u64().unwrap() > 0, "{metric}");
            }
            assert_eq!(report["embedding_identity"]["unchanged"], true);
            assert_eq!(report["embedding_identity"]["after_reopen"], true);
            assert!(report["cold_open"]["cache"]
                .as_str()
                .unwrap()
                .starts_with("warm"));
            assert!(
                report["add"]["count"].as_u64().unwrap() > 0,
                "real adds timed"
            );
            assert!(report["cold_open"]["child"]["first_query_ms"].is_number());
        }
        assert!(backup.is_dir(), "the backup itself is left alone");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// D2 grows the loaded store to its total.
    #[cfg(unix)] // real datasets need a verifiable private root
    #[test]
    fn d2_grows_d1_to_its_total() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_d2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        let dataset = Dataset::D2 {
            backup,
            total: FIXTURE + CLAIMS + 20,
        };
        let report = measure(
            ENGINE,
            Mode::Bench,
            &dataset,
            &root.join("runs"),
            Isolation::InProcess,
        )
        .unwrap();
        assert_eq!(report["grown"]["memories"], 20);
        assert_eq!(report["memories"], FIXTURE + CLAIMS + 20);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_backup_is_refused_before_any_directory_is_made() {
        let root = std::env::temp_dir().join(format!("store_bench_nob_{}", std::process::id()));
        let dataset = Dataset::D1(root.join("no-such-backup"));
        assert!(measure(ENGINE, Mode::Bench, &dataset, &root, Isolation::InProcess).is_err());
        assert!(!root.exists());
    }

    /// A run root inside the backup, or a backup inside the root, is
    /// refused before anything is written.
    #[test]
    fn a_root_overlapping_the_backup_is_refused() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_overlap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        let inside = backup.join("runs");
        let dataset = Dataset::D1(backup.clone());
        for runs in [&root, &inside] {
            assert!(measure(ENGINE, Mode::Bench, &dataset, runs, Isolation::InProcess).is_err());
        }
        assert!(!inside.exists());
        // A root reached through a symlink into the backup, existing or not.
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&backup, &link).unwrap();
            for runs in [link.clone(), link.join("runs")] {
                assert!(
                    measure(ENGINE, Mode::Bench, &dataset, &runs, Isolation::InProcess).is_err(),
                    "{}",
                    runs.display()
                );
            }
            assert!(!backup.join("runs").exists());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// D3 ingests its pages with readers searching alongside from the first
    /// page to the last, after an idle run on the same inputs, and every
    /// page body lands in the blob store.
    #[cfg(unix)] // real datasets need a verifiable private root
    #[test]
    fn d3_ingests_its_pages_beside_the_readers() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_d3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        let dataset = Dataset::D3 {
            backup,
            pages: 1_500,
        };
        let report = measure(
            SQLITE,
            Mode::Bench,
            &dataset,
            &root.join("runs"),
            Isolation::InProcess,
        )
        .unwrap();
        let ingest = &report["ingest"];
        assert_eq!(ingest["pages"], 1_500);
        // Every page went in beside the readers, and they searched to the
        // last page: no part of the timed ingest ran without search load.
        assert_eq!(ingest["readers_beside_ingest"]["writer_adds"], 1_500);
        let coverage = ingest["reader_coverage_of_ingest"].as_f64().unwrap();
        assert!(coverage >= 0.95, "{coverage}");
        assert!(ingest["hybrid_p95_ratio"].is_number());
        // G7 and G3 on the ingested store, before the other workloads.
        assert!(report["cold_open_after_ingest"]["child"]["first_query_ms"].is_number());
        assert!(report["cold_open"]["child"]["first_query_ms"].is_number());
        assert!(
            report["g5_idle_ratio"].is_null(),
            "no child sample in process"
        );
        assert!(report["disk_mb_after_ingest"]["blobs"].as_f64().unwrap() > 10.0);
        assert_eq!(report["restore"]["matched"], true);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// G2's golden set runs through the eval test's own code on a
    /// candidate against the engine, and the engine matches its baseline.
    #[test]
    fn the_golden_set_scores_a_candidate_against_the_engine() {
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_g2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let report = harness::relevance_run(SQLITE, "golden", &root, None).unwrap();
        let golden = &report["golden"];
        assert_eq!(golden["valid"], true, "{golden}");
        assert_eq!(
            golden["cells"].as_object().unwrap().len(),
            8,
            "keyword and hashed, 4 strategies"
        );
        assert_eq!(
            golden["reported"].as_object().unwrap().len(),
            8,
            "the +dates modes"
        );
        assert!(golden["misses"].is_array() && golden["passed"].is_boolean());
        assert_eq!(report["gates"], serde_json::json!(["G2"]));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The #1563 set on D1: vectors checked against the store's identity,
    /// missing relevant ids left out, a query with none dropped by ordinal.
    #[cfg(unix)]
    #[test]
    fn the_real_set_scores_d1_and_drops_queries_it_cannot_hold() {
        use remind_me_core::db::queries;
        use remind_me_core::db::vectors::Vectors;
        use remind_me_core::models::MemoryListInput;
        let _serial = harness::serial();
        let root = std::env::temp_dir().join(format!("store_bench_1563_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        // Each memory's vector points its own way (distinct angles in a
        // plane), and each query's is its memory's, so no two memories tie
        // in the vector stage and the engine ranks the same way twice.
        let unit = |j: usize| -> Vec<f32> {
            let a = (j as f32).sqrt() * 0.1;
            let mut v = vec![0.0_f32; 8];
            (v[0], v[1]) = (a.cos(), a.sin());
            v
        };
        let mut vectors = std::collections::HashMap::new();
        let picked: Vec<(String, String)> = {
            let db = remind_me_core::Database::open_backend(
                Box::new(EngineTables::open(&backup).unwrap()),
                None,
            )
            .unwrap();
            let store = db.store();
            let v = Vectors::new(&store);
            for (k, value) in [("backend", "test"), ("model", "fixture"), ("dim", "8")] {
                v.set_meta(k, value, "2026-11-23").unwrap();
            }
            let mut ids = Vec::new();
            loop {
                let input = MemoryListInput {
                    limit: 100,
                    offset: ids.len(),
                    ..MemoryListInput::default()
                };
                let page = queries::list_memories(&store, &input).unwrap().memories;
                if page.is_empty() {
                    break;
                }
                ids.extend(page.into_iter().map(|m| m.id));
            }
            for (j, id) in ids.into_iter().enumerate() {
                let bytes: Vec<u8> = unit(j).iter().flat_map(|x| x.to_le_bytes()).collect();
                v.put(&id, 0, &bytes).unwrap();
                vectors.insert(id, unit(j));
            }
            let input = MemoryListInput {
                limit: 3,
                category: Some("fact".into()),
                ..MemoryListInput::default()
            };
            queries::list_memories(&store, &input)
                .unwrap()
                .memories
                .into_iter()
                .map(|m| (m.id, m.content))
                .collect()
        };
        assert_eq!(picked.len(), 3);
        let mut q = String::new();
        let fingerprint = harness::fingerprint::backup_fingerprint(&backup).unwrap();
        let header = |sha256: &str| {
            let header = serde_json::json!({
                "backend": "test", "model": "fixture", "dim": 8, "role": "query",
                "backup": "small", "backup_sha256": sha256,
            });
            format!("{header}\n")
        };
        let mut v = header(&fingerprint);
        let lines = picked
            .iter()
            .map(|(id, content)| (content.clone(), vec![id.clone()], vectors[id].clone()))
            .chain([(
                "no such memory".to_string(),
                vec!["mem_missing".to_string()],
                unit(10_000),
            )]);
        for (n, (text, relevant, vector)) in lines.enumerate() {
            let row = serde_json::json!({ "query": text, "relevant": relevant, "kind": "real" });
            q.push_str(&format!("{row}\n"));
            let vector = serde_json::json!({ "n": n + 1, "query": text, "vector": vector });
            v.push_str(&format!("{vector}\n"));
        }
        let files = root.join("files");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("q.jsonl"), q).unwrap();
        std::fs::write(files.join("v.jsonl"), v).unwrap();
        let set = format!("d1:{}", backup.display());
        let report = harness::relevance_run(
            ENGINE,
            &set,
            &root.join("runs"),
            Some((&files.join("q.jsonl"), &files.join("v.jsonl"))),
        )
        .unwrap();
        let real = &report["real_1563"];
        assert_eq!(real["queries"], 4);
        assert_eq!(real["scored"], 3);
        assert_eq!(real["dropped"], serde_json::json!([4]));
        assert_eq!(real["relevant_ids_missing"], 1);
        assert_eq!(real["cells"].as_object().unwrap().len(), 8);
        assert_eq!(
            real["misses"],
            serde_json::json!([]),
            "the engine against itself: {real}"
        );
        // A 4-query fixture is not the 50-query source set (D55): scored,
        // but never a pass.
        assert_eq!(real["scoring_input"], false);
        assert_eq!(real["source_queries_required"], 50);
        assert_eq!(real["passed"], false);
        assert!(
            !real.to_string().contains("no such memory"),
            "no query text"
        );
        assert_eq!(report["backup_sha256"], fingerprint.as_str());

        // Vectors made for another backup are refused before scoring.
        let ours = std::fs::read_to_string(files.join("v.jsonl")).unwrap();
        let theirs = ours.replacen(&fingerprint, "0".repeat(64).as_str(), 1);
        std::fs::write(files.join("other.jsonl"), theirs).unwrap();
        let refused = harness::relevance_run(
            ENGINE,
            &set,
            &root.join("runs"),
            Some((&files.join("q.jsonl"), &files.join("other.jsonl"))),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("another backup"), "{refused}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
