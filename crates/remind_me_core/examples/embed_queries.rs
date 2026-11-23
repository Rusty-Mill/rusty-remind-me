//! Embeds the #1563 relevance set's queries for R3's G2 on D1 (D59), so
//! the relevance run itself calls no model.
//!
//! ```text
//! cargo run --release -p remind_me_core --example embed_queries -- \
//!     <backup name> <queries.jsonl> <vectors.jsonl>
//! ```
//!
//! Configure the embedder that made D1's vectors, as the node does
//! (`REMIND_ME_EMBEDDING_BACKEND` and its settings). Each query is
//! embedded as a query (`EmbedRole::Query`), in set order. The output's
//! first line names the backend, model, dimension and role, and the backup
//! it is for; each later line is `{"n", "query", "vector"}`, where `n` is
//! the query's ordinal in the set from 1 (blank lines skipped).
//! `store_bench relevance` refuses vectors whose header does not match the
//! loaded D1's embedding identity.
//!
//! The output holds the private query texts, so it is created new, never
//! overwritten, and on Unix readable by its owner only. No error names a
//! query's text.

use remind_me_core::embedder::{configured_embedder, ConfiguredEmbedder, EmbedRole, Embedder};
use serde::Deserialize;
use serde_json::json;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Queries embedded per call.
const BATCH: usize = 16;

/// One line of the #1563 set; only the text is embedded.
#[derive(Deserialize)]
struct Query {
    query: String,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [backup, queries, out] = args.as_slice() else {
        return Err("usage: embed_queries <backup name> <queries.jsonl> <vectors.jsonl>".into());
    };
    let embedder = match configured_embedder() {
        ConfiguredEmbedder::Ready(embedder) => embedder,
        ConfiguredEmbedder::Off => {
            return Err("no embedder is configured (REMIND_ME_EMBEDDING_BACKEND)".into())
        }
        ConfiguredEmbedder::Unusable { reason } => {
            return Err(format!("the configured embedder cannot embed: {reason}").into())
        }
    };
    let texts = read_queries(Path::new(queries))?;
    let file = create_private(Path::new(out))?;
    write_vectors(&*embedder, backup, &texts, file)?;
    eprintln!("embedded {} queries", texts.len());
    Ok(())
}

/// The set's query texts, in order, blank lines skipped.
fn read_queries(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)?;
    let mut texts = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let query: Query = serde_json::from_str(line)
            .map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))?;
        texts.push(query.query);
    }
    Ok(texts)
}

/// A new file at `path`, readable by its owner only on Unix; an existing
/// file is refused.
fn create_private(path: &Path) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()).into())
}

/// The header, then each text's query vector, to `out`.
fn write_vectors(
    embedder: &dyn Embedder,
    backup: &str,
    texts: &[String],
    mut out: impl Write,
) -> Result<()> {
    let id = embedder.identity();
    let header = json!({
        "backend": id.backend,
        "model": id.model,
        "dim": id.dim,
        "role": "query",
        "backup": backup,
    });
    writeln!(out, "{header}")?;
    for (b, chunk) in texts.chunks(BATCH).enumerate() {
        let first = b * BATCH + 1;
        let vectors = embedder
            .embed(chunk, EmbedRole::Query)
            .map_err(|e| format!("queries {first} on: {e}"))?;
        if vectors.len() != chunk.len() {
            return Err(format!(
                "queries {first} on: {} vectors for {} queries",
                vectors.len(),
                chunk.len()
            )
            .into());
        }
        for (i, (text, vector)) in chunk.iter().zip(vectors).enumerate() {
            let n = first + i;
            if vector.len() != id.dim {
                return Err(
                    format!("query {n}: a {}-dim vector, not {}", vector.len(), id.dim).into(),
                );
            }
            writeln!(
                out,
                "{}",
                json!({ "n": n, "query": text, "vector": vector })
            )?;
        }
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use remind_me_core::embedder::{EmbedError, EmbeddingIdentity};
    use serde_json::Value;

    /// Vectors of `dim`, each the text's length; records the role.
    struct Fake {
        dim: usize,
        roles: std::sync::Mutex<Vec<EmbedRole>>,
    }

    impl Embedder for Fake {
        fn embed(
            &self,
            texts: &[String],
            role: EmbedRole,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            self.roles.lock().unwrap().push(role);
            Ok(texts.iter().map(|t| vec![t.len() as f32; 2]).collect())
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn identity(&self) -> EmbeddingIdentity {
            EmbeddingIdentity {
                backend: "fake".into(),
                model: "m".into(),
                dim: self.dim,
            }
        }
    }

    fn fake(dim: usize) -> Fake {
        Fake {
            dim,
            roles: std::sync::Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn the_header_names_the_model_and_lines_number_the_set_from_one() {
        let texts: Vec<String> = (0..20).map(|i| "q".repeat(i + 1)).collect();
        let embedder = fake(2);
        let mut out = Vec::new();
        write_vectors(&embedder, "manual-x.engine", &texts, &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({ "backend": "fake", "model": "m", "dim": 2, "role": "query", "backup": "manual-x.engine" })
        );
        assert_eq!(lines.len(), 21);
        assert_eq!(lines[20]["n"], 20);
        assert_eq!(lines[20]["query"], texts[19]);
        assert_eq!(lines[20]["vector"], json!([20.0, 20.0]));
        let roles = embedder.roles.lock().unwrap();
        assert!(roles.iter().all(|r| matches!(r, EmbedRole::Query)));
    }

    #[test]
    fn a_vector_of_the_wrong_size_is_refused_without_its_text() {
        let error = write_vectors(&fake(3), "b", &["secret".into()], Vec::new()).unwrap_err();
        assert!(error.to_string().contains("query 1"), "{error}");
        assert!(!error.to_string().contains("secret"), "{error}");
    }

    #[test]
    fn blank_lines_are_skipped_and_an_existing_output_is_refused() {
        let dir = std::env::temp_dir().join(format!("embed_queries_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let set = dir.join("set.jsonl");
        std::fs::write(
            &set,
            "{\"query\":\"a\",\"relevant\":[]}\n\n{\"query\":\"b\"}\n",
        )
        .unwrap();
        assert_eq!(read_queries(&set).unwrap(), ["a", "b"]);
        assert!(create_private(&set).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let out = dir.join("out.jsonl");
            drop(create_private(&out).unwrap());
            let mode = std::fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
