//! `rusty-remind-me copy-store`: copy an old SQLite `memory.db` onto the
//! engine (ADR-0023 §5, ADR-0025), without touching the node. The source is
//! read-only; every row keeps its id, is verified after it is written, and a
//! row the engine cannot keep is reported and makes the command fail rather
//! than vanish.

use std::path::{Path, PathBuf};

pub const USAGE: &str =
    "Usage: rusty-remind-me copy-store --to <engine-dir> [--from <memory.db>]\n\
Copies the SQLite store (default: the configured database) into a new engine\n\
data directory. The source is never written to.";

/// What the command was asked to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyArgs {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Parse the arguments after `copy-store`, with `default_from` as the
/// source when `--from` is absent.
pub fn parse(args: &[String], default_from: &Path) -> Result<CopyArgs, String> {
    let mut from = None;
    let mut to = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.as_str() {
            "--from" => &mut from,
            "--to" => &mut to,
            other => return Err(format!("Error: unknown argument {other:?}.\n{USAGE}")),
        };
        let value = rest
            .next()
            .ok_or_else(|| format!("Error: {flag} expects a value.\n{USAGE}"))?;
        *slot = Some(PathBuf::from(value));
    }
    let to = to.ok_or_else(|| format!("Error: --to is required.\n{USAGE}"))?;
    Ok(CopyArgs {
        from: from.unwrap_or_else(|| default_from.to_path_buf()),
        to,
    })
}

/// Run the copy and print what it did. Fails when any row was refused.
pub fn run(args: &CopyArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Each table as it finishes: the copy takes minutes on a large store.
    let mut print_table = |done: remind_me_core::db::engine::copy::TableDone| {
        println!(
            "{}: {} copied in {:.1}s",
            done.table,
            done.rows,
            done.elapsed.as_secs_f64()
        );
    };
    let report =
        remind_me_core::db::engine::copy::copy_file(&args.from, &args.to, &mut print_table)?;
    if report.refused.is_empty() {
        println!("copied {} into {}", args.from.display(), args.to.display());
        return Ok(());
    }
    for refused in &report.refused {
        eprintln!(
            "refused {} [{}]: {}",
            refused.table, refused.key, refused.reason
        );
    }
    Err(format!(
        "{} row(s) could not be copied. {} was left as it was; remove the \
         partial copy in {} before trying again",
        report.refused.len(),
        args.from.display(),
        args.to.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_source_defaults_to_the_configured_database() {
        let parsed = parse(&args(&["--to", "engine"]), Path::new("memory.db")).unwrap();
        assert_eq!(
            parsed,
            CopyArgs {
                from: PathBuf::from("memory.db"),
                to: PathBuf::from("engine"),
            }
        );
        let parsed = parse(
            &args(&["--from", "old.db", "--to", "engine"]),
            Path::new("memory.db"),
        )
        .unwrap();
        assert_eq!(parsed.from, PathBuf::from("old.db"));
    }

    #[test]
    fn a_missing_target_or_value_or_a_stray_argument_is_refused() {
        let default = Path::new("memory.db");
        assert!(parse(&args(&[]), default)
            .unwrap_err()
            .contains("--to is required"));
        assert!(parse(&args(&["--to"]), default)
            .unwrap_err()
            .contains("expects a value"));
        assert!(parse(&args(&["--to", "e", "extra"]), default)
            .unwrap_err()
            .contains("unknown argument"));
    }
}
