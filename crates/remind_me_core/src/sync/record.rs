//! Conflict resolution for one incoming `memories` sync record.
//!
//! Mirrors the reference's `_upsert_one` exactly: last-write-wins on
//! `updated_at`, with two deliberate exceptions applied regardless of which
//! side wins — `tags` merge by union (dedup, order-preserving) and
//! `metadata` merges shallowly per key, the LWW winner's value winning on a
//! key collision. See `docs/adr/0004-sync-protocol-and-conflict-resolution.md`.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

fn default_category() -> String {
    "general".to_string()
}
fn default_source() -> String {
    "manual".to_string()
}
fn default_client() -> String {
    super::DEFAULT_CLIENT.to_string()
}
fn default_status() -> String {
    "active".to_string()
}
fn default_memory_type() -> String {
    "unclassified".to_string()
}
fn default_decay_rate() -> f64 {
    0.1
}
fn default_vitality() -> f64 {
    1.0
}
fn default_base_weight() -> f64 {
    1.0
}
fn default_metadata() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Accept SQLite's integer booleans as well as real JSON booleans.
///
/// A payload built by this crate's own triggers carries `0`/`1`; one built by
/// hand or by a future writer may carry `false`/`true`. Both have to work, and
/// a null has to read as false rather than as a parse error, because the field
/// is absent-by-default on older peers.
fn de_sqlite_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Bool(b) => Ok(b),
        Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        Value::Null => Ok(false),
        other => Err(serde::de::Error::custom(format!(
            "expected a boolean or 0/1 for `sensitive`, got {}",
            other
        ))),
    }
}

/// One `memories` row as it travels the wire, matching the column set this
/// crate's `memories_outbox_ai`/`memories_outbox_au` triggers snapshot into
/// `sync_outbox.payload` — see `crates/remind_me_core/src/db/schema_triggers.sql`.
///
/// Deliberately does **not** include `doc_id`/`chunk_index`: the reference's
/// own outbox payload carries them (for wire column-list parity across every
/// schema version) but its receiving side (`sync.py`'s `_upsert_one`) never
/// reads them back off an incoming record either — chunking state isn't
/// conflict-resolved over sync, only produced locally. `deleted_at` **is**
/// included: unlike `doc_id`/`chunk_index`, the reference's `_upsert_one`
/// does apply it, via the same LWW path as every other column, which is what
/// lets a tombstone propagate at all (`#76`). `sensitive` (issue #105) and
/// `remind_at` (issue #116) are included on the same grounds — both are
/// properties of the memory rather than of the machine holding it, and the
/// reference's `_upsert_one` drops both, which leaves its own outbox payload
/// carrying two fields nobody reads back. That divergence is deliberate and
/// recorded in `docs/parity-loop-decisions.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRecord {
    pub id: String,
    pub content: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default = "default_metadata")]
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub capture_id: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default = "default_client")]
    pub client: String,
    #[serde(default)]
    pub accessed_at: Option<String>,
    #[serde(default)]
    pub access_count: i64,
    #[serde(default = "default_decay_rate")]
    pub decay_rate: f64,
    #[serde(default = "default_vitality")]
    pub vitality: f64,
    #[serde(default = "default_base_weight")]
    pub base_weight: f64,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default = "default_memory_type")]
    pub memory_type: String,
    #[serde(default)]
    pub source_capture_id: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub deleted_at: Option<String>,
    /// Applied for the same reason as `deleted_at`: the payload carries it, so
    /// a peer that drops it silently unhides a memory the author marked. Not a
    /// confidentiality breach — this was never access control — but it defeats
    /// the flag's entire purpose the moment two nodes sync.
    ///
    /// The custom deserializer is not decoration. SQLite has no boolean type,
    /// so the outbox trigger's `json_object('sensitive', NEW.sensitive)` emits
    /// the integer `0` or `1` — and serde refuses to read an integer into a
    /// `bool`. With a plain `#[serde(default)]` every *memory* record in a push
    /// batch failed to deserialise, the receiving node counted them all as
    /// failures, and `push_outbox` reported `pushed: 0` while the hub stayed
    /// empty. Sync stopped working entirely, and nothing local looked wrong.
    ///
    /// `default` still matters on top of that: a record from a node predating
    /// the v27 schema has no `sensitive` key at all, and must read as
    /// not-sensitive rather than failing the whole pull.
    #[serde(default, deserialize_with = "de_sqlite_bool")]
    pub sensitive: bool,
    /// Applied for the same reason as `sensitive`: the payload carries it, so
    /// dropping it on receipt would leave a reminder set on your laptop
    /// invisible to your desktop, while every other property of the same
    /// memory arrived intact — the kind of half-wiring that reads as a bug
    /// long before anyone finds the missing column.
    ///
    /// One consequence is deliberate: `reminder_deliveries` is local, so a
    /// reminder that has already fired on one node is still pending on
    /// another and will fire there too. That is the right default for a
    /// reminder — being told twice on two machines beats being told on
    /// neither because the machine that fired it was the one you weren't at.
    #[serde(default)]
    pub remind_at: Option<String>,
}

#[derive(Debug)]
pub struct SyncApplyError(pub String);

impl std::fmt::Display for SyncApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SyncApplyError {}
impl From<rusqlite::Error> for SyncApplyError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

/// What applying one record did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The incoming record lost LWW (or tied). At most a tags/metadata-only
    /// update happened; `updated_at` and every other column were left
    /// untouched, and there is nothing to re-embed.
    NotApplied,
    /// The incoming record won: inserted as a new row or fully updated an
    /// existing one. Carries the memory's rowid so the caller can re-embed.
    Applied { rowid: i64 },
}

/// Canonicalize a timestamp to a fixed-width UTC RFC3339 string, so plain
/// string comparison is a valid `updated_at` ordering. A value this crate
/// cannot parse is left as-is rather than rejected outright — every
/// timestamp this crate itself ever writes is already in this exact shape,
/// so this only matters for a record from a source with a looser format,
/// and failing softly here matches the reference's own tolerance.
pub(super) fn canon_ts(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
        .unwrap_or_else(|_| raw.to_string())
}

/// Union-merge, deduplicated, order-preserving: every local tag first, then
/// any incoming tag not already present. Applied regardless of which side
/// wins LWW — the same idiom `upsert_entity` already uses for aliases.
fn merge_tags(local: &[String], incoming: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(local.len() + incoming.len());
    for tag in local.iter().chain(incoming.iter()) {
        if seen.insert(tag.clone()) {
            merged.push(tag.clone());
        }
    }
    merged
}

/// Shallow per-key merge: start from the LWW loser's object, then let the
/// winner's keys overwrite on collision. Deliberately not recursive — a key
/// holding a nested object/array is replaced wholesale by the winner's
/// value, never deep-merged, matching the reference exactly.
fn merge_metadata(local: &Value, incoming: &Value, incoming_wins: bool) -> Value {
    let (winner, loser) = if incoming_wins {
        (incoming, local)
    } else {
        (local, incoming)
    };
    let mut merged = loser.as_object().cloned().unwrap_or_default();
    if let Some(winner_obj) = winner.as_object() {
        for (key, value) in winner_obj {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

struct LocalRow {
    tags: Vec<String>,
    metadata: Value,
    updated_at: String,
}

fn fetch_local(conn: &Connection, id: &str) -> rusqlite::Result<Option<LocalRow>> {
    conn.query_row(
        "SELECT tags, metadata, updated_at FROM memories WHERE id = ?",
        params![id],
        |row| {
            let tags_json: String = row.get(0)?;
            let metadata_json: String = row.get(1)?;
            Ok(LocalRow {
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                metadata: serde_json::from_str(&metadata_json)
                    .unwrap_or_else(|_| default_metadata()),
                updated_at: row.get(2)?,
            })
        },
    )
    .optional()
}

/// Apply one incoming sync record: last-write-wins on `updated_at` (a tie
/// means the incoming side loses -- it must be *strictly* newer to win),
/// with `tags`/`metadata` merged regardless of the outcome.
///
/// Any `sync_outbox` row this write itself creates (for this memory) is
/// immediately marked sent -- echo suppression, so the next push cycle
/// does not hand the remote back the very change it just sent us. A
/// concurrent, genuinely local edit to the same memory is untouched: it
/// can only have created outbox rows *before* this call's snapshot, which
/// is exactly what scopes the suppression to this write's own echo.
pub fn upsert_record(
    conn: &Connection,
    record: &SyncRecord,
) -> Result<ApplyOutcome, SyncApplyError> {
    if record.id.trim().is_empty()
        || record.content.is_empty()
        || record.created_at.trim().is_empty()
        || record.updated_at.trim().is_empty()
    {
        return Err(SyncApplyError(
            "record is missing a required field (id/content/created_at/updated_at)".to_string(),
        ));
    }

    let updated_at = canon_ts(&record.updated_at);
    let created_at = canon_ts(&record.created_at);
    let accessed_at = record
        .accessed_at
        .as_deref()
        .map(canon_ts)
        .unwrap_or_else(|| created_at.clone());

    let local = fetch_local(conn, &record.id)?;
    let incoming_wins = match &local {
        None => true,
        Some(local) => updated_at > local.updated_at,
    };

    let before_outbox_id: i64 =
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM sync_outbox", [], |r| {
            r.get(0)
        })?;

    let outcome = if incoming_wins {
        let local_tags: &[String] = local.as_ref().map(|l| l.tags.as_slice()).unwrap_or(&[]);
        let local_metadata = local
            .as_ref()
            .map(|l| l.metadata.clone())
            .unwrap_or_else(default_metadata);
        let merged_tags = merge_tags(local_tags, &record.tags);
        let merged_metadata = merge_metadata(&local_metadata, &record.metadata, true);

        conn.execute(
            "INSERT INTO memories (
                id, content, category, tags, source, metadata, created_at, updated_at,
                capture_id, node_id, client, accessed_at, access_count, decay_rate,
                vitality, base_weight, status, memory_type, source_capture_id,
                subject, predicate, object, superseded_by, deleted_at, sensitive,
                remind_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                category = excluded.category,
                tags = excluded.tags,
                source = excluded.source,
                metadata = excluded.metadata,
                updated_at = excluded.updated_at,
                capture_id = excluded.capture_id,
                node_id = excluded.node_id,
                client = excluded.client,
                accessed_at = excluded.accessed_at,
                access_count = excluded.access_count,
                decay_rate = excluded.decay_rate,
                vitality = excluded.vitality,
                base_weight = excluded.base_weight,
                status = excluded.status,
                memory_type = excluded.memory_type,
                source_capture_id = excluded.source_capture_id,
                subject = excluded.subject,
                predicate = excluded.predicate,
                object = excluded.object,
                superseded_by = excluded.superseded_by,
                deleted_at = excluded.deleted_at,
                sensitive = excluded.sensitive,
                remind_at = excluded.remind_at",
            params![
                record.id,
                record.content,
                record.category,
                serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string()),
                record.source,
                merged_metadata.to_string(),
                created_at,
                updated_at,
                record.capture_id,
                record.node_id,
                record.client,
                accessed_at,
                record.access_count,
                record.decay_rate,
                record.vitality,
                record.base_weight,
                record.status,
                record.memory_type,
                record.source_capture_id,
                record.subject,
                record.predicate,
                record.object,
                record.superseded_by,
                record.deleted_at,
                record.sensitive,
                record.remind_at,
            ],
        )?;
        let rowid: i64 = conn.query_row(
            "SELECT rowid FROM memories WHERE id = ?",
            params![record.id],
            |r| r.get(0),
        )?;
        ApplyOutcome::Applied { rowid }
    } else {
        let local = local.expect("incoming_wins is false only when a local row was found");
        let merged_tags = merge_tags(&local.tags, &record.tags);
        let merged_metadata = merge_metadata(&local.metadata, &record.metadata, false);

        if merged_tags != local.tags || merged_metadata != local.metadata {
            conn.execute(
                "UPDATE memories SET tags = ?, metadata = ? WHERE id = ?",
                params![
                    serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string()),
                    merged_metadata.to_string(),
                    record.id,
                ],
            )?;
        }
        ApplyOutcome::NotApplied
    };

    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE sync_outbox SET sent_at = ? WHERE id > ? AND memory_id = ? AND sent_at = ''",
        params![now, before_outbox_id, record.id],
    )?;

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_tags_unions_local_first_then_new_incoming_tags_deduped() {
        let merged = merge_tags(
            &["a".to_string(), "b".to_string()],
            &["b".to_string(), "c".to_string()],
        );
        assert_eq!(merged, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_metadata_shallow_merges_winner_over_loser_not_recursively() {
        let local =
            serde_json::json!({"shared": "local-value", "local_only": "kept", "nested": {"a": 1}});
        let incoming = serde_json::json!({"shared": "remote-value", "remote_only": "kept", "nested": {"b": 2}});

        let merged = merge_metadata(&local, &incoming, true);

        assert_eq!(
            merged,
            serde_json::json!({
                "shared": "remote-value",
                "local_only": "kept",
                "remote_only": "kept",
                "nested": {"b": 2},
            }),
            "the winner's nested value replaces the loser's wholesale -- not deep-merged"
        );
    }

    #[test]
    fn merge_metadata_when_incoming_loses_local_keeps_the_shared_key() {
        let local = serde_json::json!({"shared": "local-value"});
        let incoming = serde_json::json!({"shared": "remote-value", "remote_only": "kept"});

        let merged = merge_metadata(&local, &incoming, false);

        assert_eq!(
            merged,
            serde_json::json!({"shared": "local-value", "remote_only": "kept"})
        );
    }

    #[test]
    fn canon_ts_normalizes_a_non_utc_offset_to_utc() {
        assert_eq!(
            canon_ts("2026-01-01T12:00:00+02:00"),
            "2026-01-01T10:00:00+00:00"
        );
    }

    #[test]
    fn canon_ts_leaves_an_unparseable_timestamp_as_is() {
        assert_eq!(canon_ts("not-a-timestamp"), "not-a-timestamp");
    }
}
