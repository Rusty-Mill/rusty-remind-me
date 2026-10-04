//! Attachment references: a memory points at a file or URL by fingerprint.
//!
//! The bytes are never stored. A path is hashed (`sha256:<hex>`) so the same
//! file attached to two memories meets in the reference index even if it
//! moves; a URL is stored as given. Resolution happens before the memory is
//! written, so a missing file fails the whole add rather than half of it.

use crate::db::{Result, StoreError};
use crate::models::AttachmentInput;
use serde_json::{json, Value};
use std::path::Path;

/// An attachment with its fingerprint computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAttachment {
    /// `sha256:<hex>`, or the URL itself.
    pub value: String,
    /// The caller's label, else the file name, else the URL.
    pub label: String,
    /// What goes into `metadata.attachments[]`.
    pub metadata: Value,
}

/// MIME type by extension; `application/octet-stream` when unknown.
pub fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("txt" | "log") => "text/plain",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

fn invalid(why: String) -> StoreError {
    StoreError::Invalid(why)
}

fn resolve_path(path: &str, label: Option<&str>) -> Result<ResolvedAttachment> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p).map_err(|e| invalid(format!("attachment path {path}: {e}")))?;
    if !meta.is_file() {
        return Err(invalid(format!("attachment path {path} is not a file")));
    }
    let hash = sha256::try_digest(p).map_err(|e| invalid(format!("attachment path {path}: {e}")))?;
    let value = format!("sha256:{hash}");
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();
    Ok(ResolvedAttachment {
        label: label.map(str::to_string).unwrap_or(name),
        metadata: json!({
            "path": path,
            "size": meta.len(),
            "mime": mime_for(p),
            "hash": value,
        }),
        value,
    })
}

fn resolve_url(url: &str, label: Option<&str>) -> Result<ResolvedAttachment> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(invalid(format!("attachment url {url} must be http(s)")));
    }
    Ok(ResolvedAttachment {
        value: url.to_string(),
        label: label.map(str::to_string).unwrap_or_else(|| url.to_string()),
        metadata: json!({ "url": url }),
    })
}

/// Resolve every attachment, failing on the first bad one with an error that
/// names it. An entry with neither or both of `path` and `url` is invalid.
pub fn resolve(inputs: &[AttachmentInput]) -> Result<Vec<ResolvedAttachment>> {
    inputs
        .iter()
        .map(|a| {
            let label = a.label.as_deref().filter(|l| !l.is_empty());
            match (a.path.as_deref(), a.url.as_deref()) {
                (Some(path), None) => resolve_path(path, label),
                (None, Some(url)) => resolve_url(url, label),
                _ => Err(invalid(
                    "attachment needs exactly one of `path` or `url`".to_string(),
                )),
            }
        })
        .collect()
}

/// Record the attachments under `metadata.attachments`.
pub fn merge_metadata(metadata: &mut Value, resolved: &[ResolvedAttachment]) {
    if resolved.is_empty() {
        return;
    }
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata["attachments"] = Value::Array(resolved.iter().map(|r| r.metadata.clone()).collect());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_input(path: &str, label: Option<&str>) -> AttachmentInput {
        AttachmentInput {
            path: Some(path.to_string()),
            label: label.map(str::to_string),
            ..Default::default()
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rrm_att_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_is_hashed_and_labelled_by_name() {
        let file = tempdir().join("notes.txt");
        std::fs::write(&file, b"hello").unwrap();
        let r = resolve(&[path_input(file.to_str().unwrap(), None)]).unwrap();
        assert_eq!(
            r[0].value,
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(r[0].label, "notes.txt");
        assert_eq!(r[0].metadata["size"], 5);
        assert_eq!(r[0].metadata["mime"], "text/plain");
        let again = resolve(&[path_input(file.to_str().unwrap(), Some("mine"))]).unwrap();
        assert_eq!(again[0].label, "mine");
        assert_eq!(again[0].value, r[0].value);
    }

    #[test]
    fn a_missing_file_names_its_path() {
        let err = resolve(&[path_input("/no/such/file.bin", None)]).unwrap_err();
        assert!(err.to_string().contains("/no/such/file.bin"), "{err}");
    }

    #[test]
    fn urls_are_kept_and_validated() {
        let ok = AttachmentInput {
            url: Some("https://example.org/a.pdf".into()),
            ..Default::default()
        };
        assert_eq!(resolve(&[ok]).unwrap()[0].value, "https://example.org/a.pdf");
        let bad = AttachmentInput {
            url: Some("ftp://x".into()),
            ..Default::default()
        };
        assert!(resolve(&[bad]).is_err());
        assert!(resolve(&[AttachmentInput::default()]).is_err());
    }

    #[test]
    fn metadata_is_merged_into_an_object() {
        let mut meta = Value::Null;
        let r = resolve_url("https://e.org/x", None).unwrap();
        merge_metadata(&mut meta, &[r]);
        assert_eq!(meta["attachments"][0]["url"], "https://e.org/x");
        let mut untouched = json!({"a": 1});
        merge_metadata(&mut untouched, &[]);
        assert_eq!(untouched, json!({"a": 1}));
    }
}
