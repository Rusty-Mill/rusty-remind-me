//! `rusty-remind-me-hub-copy`: copy a SQLite or Postgres hub into an
//! embedded-engine data directory (docs/adr/0021, phase 3).
//!
//! ```text
//! rusty-remind-me-hub-copy --from-sqlite PATH  --to DIR [--check] [--drop-invalid]
//! rusty-remind-me-hub-copy --from-postgres     --to DIR [--check] [--drop-invalid]
//! ```
//!
//! `--from-postgres` reads the connection string from `DATABASE_URL`, not
//! from the command line, where any local user could read its password.
//!
//! Stop the hub first. The copy keeps every `hub_seq` and `origin_node`, so
//! nodes carry on from their cursors against the new hub. Rows the engine
//! cannot store (ids over 64 bytes or holding NUL, unreadable timestamps)
//! are listed and nothing is written, unless `--drop-invalid` says to copy
//! everything else. `--check` lists them and writes nothing either way.
//! After writing, every row is read back and compared with the source.

use remind_me_hub::import;
use remind_me_hub::store::multimodal::snapshot::{Rejected, Snapshot};
use remind_me_hub::store::multimodal::MultimodalHubStore;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: rusty-remind-me-hub-copy (--from-sqlite PATH | --from-postgres) \
                     --to DIR [--check] [--drop-invalid]\n\
                     --from-postgres reads the connection string from DATABASE_URL.";

/// Where the rows come from.
enum Source {
    Sqlite(PathBuf),
    Postgres,
}

struct Args {
    source: Source,
    target: PathBuf,
    check_only: bool,
    drop_invalid: bool,
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("copy: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(mut raw: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut source = None;
    let mut target = None;
    let mut check_only = false;
    let mut drop_invalid = false;
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--from-sqlite" => {
                let path = raw.next().ok_or("--from-sqlite needs a path")?;
                set_once(&mut source, Source::Sqlite(path.into()))?;
            }
            "--from-postgres" => set_once(&mut source, Source::Postgres)?,
            "--to" => {
                let dir = raw.next().ok_or("--to needs a directory")?;
                target = Some(PathBuf::from(dir));
            }
            "--check" => check_only = true,
            "--drop-invalid" => drop_invalid = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args {
        source: source.ok_or("name a source: --from-sqlite PATH or --from-postgres")?,
        target: target.ok_or("name the target with --to DIR")?,
        check_only,
        drop_invalid,
    })
}

fn set_once(slot: &mut Option<Source>, source: Source) -> Result<(), String> {
    if slot.replace(source).is_some() {
        return Err("name exactly one source".to_string());
    }
    Ok(())
}

fn read(source: &Source) -> Result<Snapshot, String> {
    match source {
        Source::Sqlite(path) => import::sqlite::read(path).map_err(|e| e.0),
        Source::Postgres => read_postgres(),
    }
}

#[cfg(feature = "postgres-import")]
fn read_postgres() -> Result<Snapshot, String> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or("--from-postgres reads the connection string from DATABASE_URL, which is unset")?;
    import::postgres::read(&url).map_err(|e| e.0)
}

#[cfg(not(feature = "postgres-import"))]
fn read_postgres() -> Result<Snapshot, String> {
    Err("this binary was built without the `postgres-import` feature".to_string())
}

fn run(args: &Args) -> Result<(), String> {
    if !args.check_only {
        MultimodalHubStore::check_copy_target(&args.target).map_err(|e| e.0)?;
    }
    let snapshot = read(&args.source)?;
    eprintln!(
        "copy: read {} memories, {} entities, {} links, {} relations; highest hub_seq {}",
        snapshot.memories.len(),
        snapshot.entities.len(),
        snapshot.links.len(),
        snapshot.relations.len(),
        snapshot.seq_floor(),
    );

    let rejected = snapshot.validate();
    report(&rejected);
    if args.check_only {
        return if rejected.is_empty() {
            eprintln!("copy: every row can be copied");
            Ok(())
        } else {
            Err(format!(
                "{} rows cannot be copied; nothing written",
                rejected.len()
            ))
        };
    }
    let snapshot = match (rejected.is_empty(), args.drop_invalid) {
        (true, _) => snapshot,
        (false, true) => {
            eprintln!("copy: --drop-invalid: copying without the rows listed above");
            snapshot.without_rejected().0
        }
        (false, false) => {
            return Err(format!(
                "{} rows cannot be copied; nothing written. Rerun with --drop-invalid to copy \
                 everything else",
                rejected.len()
            ))
        }
    };

    let store =
        MultimodalHubStore::create_from_snapshot(&args.target, &snapshot).map_err(|e| e.0)?;
    import::verify(&snapshot, &store).map_err(|e| format!("verification failed: {}", e.0))?;
    eprintln!(
        "copy: wrote and verified {}; set REMIND_ME_HUB_DATA_DIR to it to serve it",
        args.target.display()
    );
    Ok(())
}

fn report(rejected: &[Rejected]) {
    for row in rejected {
        eprintln!("copy: cannot copy {row}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn exactly_one_source_and_a_target_are_required() {
        assert!(parse(&["--to", "d"]).is_err());
        assert!(parse(&["--from-postgres"]).is_err());
        assert!(parse(&["--from-postgres", "--from-sqlite", "x", "--to", "d"]).is_err());
        assert!(parse(&["--from-sqlite"]).is_err());
        assert!(parse(&["--from-postgres", "--to", "d", "--frob"]).is_err());
        let args = parse(&["--from-sqlite", "hub.db", "--to", "d", "--drop-invalid"]).unwrap();
        assert!(matches!(args.source, Source::Sqlite(ref p) if p == &PathBuf::from("hub.db")));
        assert!(args.drop_invalid && !args.check_only);
    }
}
