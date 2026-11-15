//! The provenance link from a memory to the raw artifact it came from.
//!
//! A memory imported or distilled from a raw artifact (a DBS item today; a
//! clipped page or a verbatim dialog once the artifact store exists, Mill
//! Board #762) carries one [`SourceArtifact`] under
//! `metadata.source_artifact`. It lives in metadata, not in a column, because
//! memory rows are positional on the engine and a new column is a migration.
//!
//! The link names the artifact by where it lives (`store`), its identity
//! there (`id`) and the hash of the bytes the memory was built from (`hash`),
//! so a reader can tell when the artifact has changed since.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The metadata key a [`SourceArtifact`] is stored under.
pub const METADATA_KEY: &str = "source_artifact";

/// Where a memory's raw original lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceArtifact {
    /// The store holding the artifact, such as `dbs:raindrop`.
    pub store: String,
    /// The artifact's identity inside that store.
    pub id: String,
    /// The content hash of the artifact version the memory was built from.
    pub hash: String,
}

impl SourceArtifact {
    /// The link for an item read from a DBS archive: its source, its id at
    /// that source, and DBS's content hash for the version imported.
    pub fn dbs(source_name: &str, external_id: &str, content_hash: &str) -> Self {
        Self {
            store: format!("dbs:{source_name}"),
            id: external_id.to_string(),
            hash: content_hash.to_string(),
        }
    }

    /// The JSON value stored under [`METADATA_KEY`].
    pub fn to_value(&self) -> Value {
        json!({ "store": self.store, "id": self.id, "hash": self.hash })
    }

    /// The link a memory's metadata carries, if any. A malformed value reads
    /// as no link rather than an error: metadata is free-form, and a caller
    /// asking "where did this come from" should get an answer for the rest.
    pub fn from_metadata(metadata: &Value) -> Option<Self> {
        serde_json::from_value(metadata.get(METADATA_KEY)?.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbs_link_round_trips_through_metadata() {
        let link = SourceArtifact::dbs("raindrop", "x/1", "hash-1");
        let metadata = json!({ "other": 1, METADATA_KEY: link.to_value() });

        assert_eq!(link.store, "dbs:raindrop");
        assert_eq!(link.id, "x/1");
        assert_eq!(SourceArtifact::from_metadata(&metadata), Some(link));
    }

    #[test]
    fn missing_or_malformed_link_reads_as_none() {
        assert_eq!(SourceArtifact::from_metadata(&json!({})), None);
        assert_eq!(SourceArtifact::from_metadata(&Value::Null), None);
        assert_eq!(
            SourceArtifact::from_metadata(&json!({ METADATA_KEY: { "store": "dbs:x" } })),
            None
        );
    }
}
