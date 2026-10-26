//! Give the proposal rules something to see for projects that predate them
//! (ADR-0027 section 7).
//!
//! The rules read captured context, so a project discussed before remind-me
//! recorded `git_remote` and `cwd` has nothing behind it. This walks a
//! checkout and, for each crate directory that matches a known entity, saves
//! one `reference` memory naming the repository and the path. That memory is
//! then ordinary evidence, and the `built` and `path` proposals follow from
//! it.
//!
//! Nothing is promoted and nothing is migrated: the only writes are those
//! reference memories, each deletable like any other, and a person still
//! confirms every proposal they produce.

use crate::db::memories::{Memories, NewMemory};
use crate::db::{Result, Store, StoreError};
use crate::entity::{link_memory_entity, resolve_entity, Entity};
use crate::project::{looks_secret, RepoPath, RepoUrl};
use chrono::Utc;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Tag on a memory this module writes.
pub const SEED_TAG: &str = "project-backfill";
const SOURCE: &str = "project-backfill";
/// Directories under the repository root that hold crates.
const CRATE_ROOT: &str = "crates";

/// A crate directory that matches an entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Seed {
    /// The entity's canonical name.
    pub project: String,
    pub repo: RepoUrl,
    pub path: RepoPath,
    /// The memory written for it, absent on a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
}

/// What a backfill found, and wrote when asked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackfillReport {
    pub seeds: Vec<Seed>,
    /// Crate directories no entity answers to, so nothing was written.
    pub unmatched: Vec<String>,
    /// Crate directories skipped because their path looks like a secret,
    /// shown redacted. A seed stores the path verbatim, so none is written.
    pub refused: Vec<String>,
    pub dry_run: bool,
}

/// Find the crate directories of `repo_root` that match a known entity.
///
/// `repo` is the repository the checkout belongs to; `write` decides whether
/// the evidence memories are saved. Ordered by path, so two runs over an
/// unchanged checkout report the same thing.
pub fn backfill(
    store: &Store<'_>,
    repo_root: &Path,
    repo: &RepoUrl,
    write: bool,
) -> Result<BackfillReport> {
    if looks_secret(repo.as_str()) {
        return Err(StoreError::Invalid(
            "repository URL looks like a secret".into(),
        ));
    }
    let mut report = BackfillReport {
        dry_run: !write,
        ..BackfillReport::default()
    };
    for dir in crate_dirs(repo_root)? {
        let Ok(relative) = dir.strip_prefix(repo_root) else {
            continue;
        };
        let Some(path) = relative.to_str().map(|p| p.replace('\\', "/")) else {
            continue;
        };
        if looks_secret(&path) || looks_secret(&dir.display().to_string()) {
            report.refused.push(redacted(&path));
            continue;
        }
        let Ok(path) = RepoPath::parse(&path) else {
            continue;
        };
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();

        let Some(entity) = resolve_entity(store, name)? else {
            report.unmatched.push(path.as_str().to_string());
            continue;
        };
        let memory_id = write
            .then(|| write_seed(store, &entity, repo, &path, &dir))
            .transpose()?;
        report.seeds.push(Seed {
            project: entity.name,
            repo: repo.clone(),
            path,
            memory_id,
        });
    }
    report
        .seeds
        .sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    report.unmatched.sort();
    report.refused.sort();
    Ok(report)
}

fn redacted(text: &str) -> String {
    crate::boundary::scrub(text, &mut Vec::new(), &mut serde_json::Value::Null)
}

/// `crates/<group>/<name>` directories, the layout this workspace uses.
fn crate_dirs(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let root = repo_root.join(CRATE_ROOT);
    let read = |dir: &Path| -> Result<Vec<PathBuf>> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| StoreError::Invalid(format!("cannot read {}: {e}", dir.display())))?;
        let mut found: Vec<PathBuf> = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        found.sort();
        Ok(found)
    };
    if !root.is_dir() {
        return Err(StoreError::Invalid(format!(
            "{} has no {CRATE_ROOT} directory",
            repo_root.display()
        )));
    }
    let mut dirs = Vec::new();
    for group in read(&root)? {
        // Only a crate, not the group: `crates/apps` is not a project.
        dirs.extend(
            read(&group)?
                .into_iter()
                .filter(|dir| dir.join("Cargo.toml").is_file()),
        );
    }
    Ok(dirs)
}

/// One `reference` memory recording where a project lives, linked to it.
fn write_seed(
    store: &Store<'_>,
    entity: &Entity,
    repo: &RepoUrl,
    path: &RepoPath,
    dir: &Path,
) -> Result<String> {
    let now = Utc::now().to_rfc3339();
    let mut tags = vec![SEED_TAG.to_string()];
    let mut metadata = serde_json::json!({
        "project_backfill": { "repo": repo.as_str(), "path": path.as_str() }
    });
    // The common boundary, as on every write; the caller has already
    // refused secret-shaped paths and remotes, which are stored verbatim.
    let content = crate::boundary::scrub(
        &format!(
            "{} lives at {} in {}",
            entity.name,
            path.as_str(),
            repo.as_str()
        ),
        &mut tags,
        &mut metadata,
    );
    let row = NewMemory {
        category: crate::kinds::MemoryKind::Reference.as_str().to_string(),
        memory_type: crate::kinds::MemoryKind::Reference.as_str().to_string(),
        tags,
        source: SOURCE.to_string(),
        // The same two context fields a session would have recorded, so the
        // `built` and `path` rules read this exactly as they read a session.
        git_remote: Some(repo.as_str().to_string()),
        cwd: Some(dir.display().to_string()),
        metadata,
        ..crate::context::default_provenance().stamp(NewMemory::new(
            format!("mem_{}", uuid::Uuid::new_v4().simple()),
            content,
            &now,
        ))
    };
    Memories::new(store).insert(&row)?;
    link_memory_entity(store, &row.id, &entity.id)?;
    Ok(row.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::entity::upsert_entity;
    use crate::models::EntityInput;
    use crate::project_proposals::proposals;

    /// A checkout with `crates/<group>/<name>/Cargo.toml` for each name, plus
    /// a group directory and a non-crate directory that must be ignored.
    fn checkout(name: &str, crates: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rrm_backfill_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (group, crate_name) in crates {
            let dir = root.join(CRATE_ROOT).join(group).join(crate_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        }
        std::fs::create_dir_all(root.join(CRATE_ROOT).join("apps").join("not_a_crate")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        root
    }

    fn repo() -> RepoUrl {
        RepoUrl::parse("https://github.com/Rusty-Mill/rusty_mill").unwrap()
    }

    #[test]
    fn a_dry_run_writes_nothing_and_names_what_is_unmatched() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        upsert_entity(
            &store,
            &EntityInput {
                name: "rusty_mux".into(),
                kind: None,
                aliases: vec![],
            },
        )
        .unwrap();
        let root = checkout("dry", &[("apps", "rusty_mux"), ("apps", "rusty_tick")]);

        let report = backfill(&store, &root, &repo(), false).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.seeds.len(), 1);
        assert_eq!(report.seeds[0].project, "rusty_mux");
        assert_eq!(report.seeds[0].path.as_str(), "crates/apps/rusty_mux");
        assert_eq!(report.seeds[0].memory_id, None);
        assert_eq!(report.unmatched, ["crates/apps/rusty_tick"]);
        assert!(Memories::new(&store).all_live().unwrap().is_empty());
        assert!(proposals(&store).unwrap().is_empty());
    }

    #[test]
    fn writing_seeds_produces_built_and_path_proposals() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        upsert_entity(
            &store,
            &EntityInput {
                name: "rusty_mill".into(),
                kind: None,
                aliases: vec![],
            },
        )
        .unwrap();
        upsert_entity(
            &store,
            &EntityInput {
                name: "rusty_mux".into(),
                kind: None,
                aliases: vec![],
            },
        )
        .unwrap();
        let root = checkout("write", &[("apps", "rusty_mux"), ("apps", "rusty_mill")]);

        let report = backfill(&store, &root, &repo(), true).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.seeds.len(), 2);
        assert!(report.seeds.iter().all(|s| s.memory_id.is_some()));

        let keys: Vec<String> = proposals(&store)
            .unwrap()
            .into_iter()
            .map(|p| p.key)
            .collect();
        // The remote names the monorepo, so only it is proposed `built`; the
        // crate inside it gets a path, nothing more.
        assert!(keys.contains(&"rusty_mill:stage:built".to_string()));
        let mux_path = "rusty_mux:path:crates/apps/rusty_mux";
        assert!(keys.contains(&mux_path.to_string()));
        assert!(!keys.contains(&"rusty_mux:stage:built".to_string()));

        // End to end: the path proposal alone creates the crate's project.
        crate::project_proposals::confirm(&store, mux_path, "human").unwrap();
        let mux = crate::project::get(&store, "rusty_mux").unwrap().unwrap();
        assert_eq!(
            mux.anchors.path.unwrap().value.as_str(),
            "crates/apps/rusty_mux"
        );
    }

    #[test]
    fn a_secret_shaped_crate_path_is_refused_not_seeded() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        upsert_entity(
            &store,
            &EntityInput {
                name: secret.into(),
                kind: None,
                aliases: vec![],
            },
        )
        .unwrap();
        let root = checkout("secret", &[("apps", secret)]);

        let report = backfill(&store, &root, &repo(), true).unwrap();
        assert!(report.seeds.is_empty());
        assert_eq!(report.refused.len(), 1);
        assert!(!report.refused[0].contains(secret));
        let all = Memories::new(&store).all_live().unwrap();
        assert!(all
            .iter()
            .all(|m| !m.content.contains(secret)
                && m.cwd.as_deref().is_none_or(|c| !c.contains(secret))));

        let bad_repo = RepoUrl::parse(&format!("https://github.com/{secret}/x")).unwrap();
        assert!(backfill(&store, &root, &bad_repo, true).is_err());
    }

    #[test]
    fn a_checkout_with_no_crates_directory_is_an_error() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let root = std::env::temp_dir().join(format!("rrm_backfill_empty_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(backfill(&store, &root, &repo(), false).is_err());
    }
}
