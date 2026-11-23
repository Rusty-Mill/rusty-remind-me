//! A D1 backup's identity (D59): the SHA-256 of its directory tree, so
//! the #1563 query vectors `embed_queries` made can be bound to the very
//! backup a relevance run loads. Shared by `store_bench` and
//! `embed_queries`.
//!
//! Errors name a fixed class only, never a path or an entry's name: the
//! backup is private input.

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The SHA-256, in hex, of the backup directory `dir`: every entry under
/// it in name order, each as its relative path, and for a file its size
/// and its own SHA-256. Anything but plain files and directories is
/// refused, as the D1 copy refuses it.
pub fn backup_fingerprint(dir: &Path) -> Result<String> {
    if !std::fs::metadata(dir).map_err(unreadable)?.is_dir() {
        return Err("the backup is not a directory".into());
    }
    let mut listing = String::new();
    walk(dir, "", &mut listing)?;
    Ok(sha256::digest(listing))
}

/// Append the entries under `root`/`rel` to `listing`, recursively.
fn walk(root: &Path, rel: &str, listing: &mut String) -> Result<()> {
    let mut entries = std::fs::read_dir(root.join(rel))
        .map_err(unreadable)?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(unreadable)?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let kind = entry.file_type().map_err(unreadable)?;
        if kind.is_dir() {
            writeln!(listing, "d {path:?}")?;
            walk(root, &path, listing)?;
        } else if kind.is_file() {
            let size = entry.metadata().map_err(unreadable)?.len();
            let hash = sha256::try_digest(entry.path().as_path()).map_err(unreadable)?;
            writeln!(listing, "f {path:?} {size} {hash}")?;
        } else {
            return Err("the backup holds an entry that is not a plain file or directory".into());
        }
    }
    Ok(())
}

fn unreadable(e: std::io::Error) -> Box<dyn Error> {
    format!("cannot read the backup ({:?})", e.kind()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fingerprint_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), "one").unwrap();
        std::fs::write(dir.join("sub").join("b"), "two").unwrap();
        dir
    }

    #[test]
    fn the_same_tree_has_the_same_fingerprint_and_any_change_another() {
        let (x, y) = (tree("x"), tree("y"));
        let fx = backup_fingerprint(&x).unwrap();
        assert_eq!(fx, backup_fingerprint(&y).unwrap(), "same content");
        std::fs::write(y.join("sub").join("b"), "TWO").unwrap();
        assert_ne!(fx, backup_fingerprint(&y).unwrap(), "a changed byte");
        std::fs::rename(y.join("sub").join("b"), y.join("sub").join("c")).unwrap();
        std::fs::write(y.join("sub").join("c"), "two").unwrap();
        assert_ne!(fx, backup_fingerprint(&y).unwrap(), "a renamed file");
        std::fs::remove_dir_all(x).unwrap();
        std::fs::remove_dir_all(y).unwrap();
    }

    #[test]
    fn errors_name_no_path() {
        let missing = std::env::temp_dir().join("fingerprint_secret_backup_missing");
        let error = backup_fingerprint(&missing).unwrap_err().to_string();
        assert!(!error.contains("secret"), "{error}");
        #[cfg(unix)]
        {
            let dir = tree("link");
            std::os::unix::fs::symlink(dir.join("a"), dir.join("secret_link")).unwrap();
            let error = backup_fingerprint(&dir).unwrap_err().to_string();
            assert!(!error.contains("secret"), "{error}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
