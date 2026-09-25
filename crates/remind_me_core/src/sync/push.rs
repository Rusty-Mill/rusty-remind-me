//! Drain the local `sync_outbox` to one remote (a hub, in this slice).
//!
//! Mirrors the reference's `_push_outbox`: pages through everything not yet
//! recorded as sent to this specific `remote_id` in `sync_sends`, POSTs each
//! page to `{url}/sync/push`, and marks rows sent according to the
//! response shape — either the exact `processed_ids` a modern peer reports,
//! or (a legacy, count-only remote) the whole page, on the reasoning that an
//! LWW-stale record would never have been accepted anyway so re-sending it
//! costs nothing but a wasted round-trip.

use super::{http, record_push};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

/// Outbox rows sent per `POST /sync/push`, bounding request size the same
/// way the reference's own `BATCH_SIZE` does.
pub const BATCH_SIZE: usize = 200;

#[derive(Debug)]
pub struct PushError(pub String);

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for PushError {}
impl From<rusqlite::Error> for PushError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<http::HttpError> for PushError {
    fn from(e: http::HttpError) -> Self {
        Self(e.to_string())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PushReport {
    pub pushed: usize,
    pub batches: usize,
}

struct OutboxRow {
    /// `sync_outbox.id` (rowid) -- what `sync_sends` records.
    id: i64,
    /// The record's own wire id, read from `payload["id"]` -- the same id a
    /// receiving peer reports back in `processed_ids`. For every record
    /// type this equals the row's own identity (a memory/entity/relation's
    /// real id, or a link's synthetic `memory_id|entity_id`) -- *not*
    /// necessarily the `sync_outbox.memory_id` column, which for a link
    /// row holds only the memory half.
    wire_id: String,
    payload: Value,
}

/// `sync_outbox.payload` stores `tags`/`metadata` (memory records) and
/// `aliases` (entity records) as JSON-encoded *strings* (the trigger's
/// `json_object(...)` call snapshots each table's own TEXT column
/// verbatim) -- double-encoded on the wire is wrong, so this decodes them
/// back into real JSON before a record is sent. A key absent from a given
/// record's payload (e.g. `aliases` on a memory record) is simply not
/// found and left alone.
fn decode_payload(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        for key in ["tags", "metadata", "aliases"] {
            if let Some(Value::String(raw)) = object.get(key) {
                if let Ok(decoded) = serde_json::from_str::<Value>(raw) {
                    object.insert(key.to_string(), decoded);
                }
            }
        }
    }
    payload
}

fn fetch_batch(
    conn: &Connection,
    remote_id: &str,
    after_id: i64,
) -> rusqlite::Result<Vec<OutboxRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, memory_id, payload FROM sync_outbox
          WHERE id > ?1 AND sent_at = ''
            AND id NOT IN (SELECT outbox_id FROM sync_sends WHERE remote_id = ?2)
          ORDER BY id ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![after_id, remote_id, BATCH_SIZE as i64], |row| {
        let memory_id_column: String = row.get(1)?;
        let payload_json: String = row.get(2)?;
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or_else(|_| json!({}));
        // `payload["id"]` is the record's real wire id for every type; the
        // `memory_id` column fallback only matters if a malformed payload
        // somehow lacks its own "id" key.
        let wire_id = payload
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or(memory_id_column);
        Ok(OutboxRow {
            id: row.get(0)?,
            wire_id,
            payload,
        })
    })?;
    rows.collect()
}

fn mark_sent(conn: &Connection, remote_id: &str, outbox_ids: &[i64]) -> rusqlite::Result<()> {
    let now = Utc::now().to_rfc3339();
    for id in outbox_ids {
        conn.execute(
            "INSERT OR REPLACE INTO sync_sends (remote_id, outbox_id, sent_at) VALUES (?, ?, ?)",
            params![remote_id, id, now],
        )?;
    }
    Ok(())
}

/// Push every not-yet-sent-to-`remote_id` outbox row to `{hub_url}/sync/push`,
/// paging until a short page confirms the outbox is drained for this remote.
pub fn push_outbox(
    conn: &Connection,
    hub_url: &str,
    secret: &str,
    node_id: &str,
    remote_id: &str,
) -> Result<PushReport, PushError> {
    let url = format!("{}/sync/push", hub_url.trim_end_matches('/'));
    let mut report = PushReport::default();
    let mut after_id = 0i64;

    loop {
        let batch = fetch_batch(conn, remote_id, after_id)?;
        if batch.is_empty() {
            break;
        }
        let batch_len = batch.len();
        after_id = batch.iter().map(|r| r.id).max().unwrap_or(after_id);

        let records: Vec<Value> = batch
            .iter()
            .map(|r| decode_payload(r.payload.clone()))
            .collect();
        let body = json!({ "node_id": node_id, "records": records }).to_string();

        let (status, response_body) = http::post_json(&url, secret, &body)?;
        if !(200..300).contains(&status) {
            return Err(PushError(format!(
                "push to {} returned {}: {}",
                url,
                status,
                response_body.trim()
            )));
        }
        record_push(conn, remote_id);
        let response: Value = serde_json::from_str(&response_body)
            .map_err(|e| PushError(format!("push response from {} was not JSON: {}", url, e)))?;

        let sent_ids: Vec<i64> = match response.get("processed_ids").and_then(Value::as_array) {
            Some(processed) => {
                let processed: std::collections::HashSet<&str> =
                    processed.iter().filter_map(Value::as_str).collect();
                batch
                    .iter()
                    .filter(|r| processed.contains(r.wire_id.as_str()))
                    .map(|r| r.id)
                    .collect()
            }
            // A legacy, count-only remote: mark the whole page sent. An
            // LWW-stale record would never have been accepted anyway, so
            // re-sending it next cycle costs nothing but a wasted round-trip.
            None => batch.iter().map(|r| r.id).collect(),
        };
        mark_sent(conn, remote_id, &sent_ids)?;

        report.pushed += sent_ids.len();
        report.batches += 1;
        if batch_len < BATCH_SIZE {
            break;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_payload_turns_double_encoded_tags_and_metadata_into_real_json() {
        let payload = json!({
            "id": "mem_1",
            "tags": "[\"a\",\"b\"]",
            "metadata": "{\"k\":\"v\"}",
        });

        let decoded = decode_payload(payload);

        assert_eq!(decoded["tags"], json!(["a", "b"]));
        assert_eq!(decoded["metadata"], json!({"k": "v"}));
    }

    #[test]
    fn decode_payload_leaves_an_already_real_json_value_alone() {
        let payload = json!({ "tags": ["already", "real"] });
        let decoded = decode_payload(payload);
        assert_eq!(decoded["tags"], json!(["already", "real"]));
    }
}
