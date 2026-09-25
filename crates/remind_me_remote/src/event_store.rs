//! In-process [`EventStore`] backing SEP-2567 (protocol version >= 2026-07-28)
//! stateless SSE resumption.
//!
//! `rmcp` 3.0.1's Streamable HTTP server only serves `GET /mcp` (the
//! resumable event stream) when either `legacy_session_mode` is on, or the
//! session manager reports an `EventStore` (`supports_stateless_replay` in
//! `tower.rs::handle`) -- see `server::build_router`'s doc for why both stay
//! on here. Per-request dispatch for 2026-07-28+ clients (`tower.rs`'s
//! discover-lifecycle branch of `handle_post`) already works against
//! `RemindMeHandler` with no event store at all, since it never needs to
//! resume anything -- one POST, one response. The store only matters for
//! `Last-Event-Id` reconnect: a client that dropped mid-stream and wants
//! exactly the events it missed, not a resend of the whole thing.
//!
//! No built-in `EventStore` ships in `rmcp` (only the trait, plus a
//! `SessionStore` doc example for the unrelated cross-instance-recovery
//! concern) -- this is a direct, minimal implementation of the trait's own
//! contract: assign each stored event a globally unique, orderable ID,
//! remember which stream it belongs to, and replay only the later events
//! from that same stream. rmcp's caller (`persist_and_forward_event`)
//! already calls `store_event` for every SSE event in both the legacy and
//! stateless paths once an event store is configured, and `handle_get`
//! already calls `replay_events_after` -- neither `server.rs` nor
//! `handler.rs` needs any other change to light this up.
//!
//! Single process, no cross-instance sharing (matches every other
//! `remind_me_remote`/`remind_me_core::remote` design choice: one
//! connector, one `Database` mutex, no distributed state) -- so a plain
//! in-memory ring buffer is the whole implementation, not a database table.
//! Capped at [`MAX_BUFFERED_EVENTS`] total (across every stream) so a
//! long-lived connector process can't accumulate unbounded memory from
//! streams nobody ever resumes; once an event ages out, replaying from it
//! degrades to an empty stream (rmcp's own `handle_get` already treats a
//! `replay_events_after` `Err` as an empty-stream fallback with a warning,
//! but an unknown ID here isn't an error condition -- it's just too old to
//! honor -- so this returns `Ok` with nothing to replay instead).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::transport::common::server_side_http::ServerSseMessage;
use rmcp::transport::streamable_http_server::session::{
    EventId, EventStore, EventStoreError, EventStream, StreamId,
};
use tokio::sync::RwLock;

/// Total buffered events retained for replay, across every stream. Once
/// exceeded, the oldest events are dropped first (a ring buffer, not a
/// per-stream quota -- simplest thing that bounds memory for a single-user
/// local process).
const MAX_BUFFERED_EVENTS: usize = 1024;

#[derive(Debug, Clone)]
struct StoredEvent {
    id: u64,
    stream_id: StreamId,
    message: ServerSseMessage,
}

/// See the module doc for why this is in-memory and single-process.
#[derive(Debug, Default)]
pub struct InProcessEventStore {
    next_id: AtomicU64,
    events: RwLock<VecDeque<StoredEvent>>,
}

impl InProcessEventStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl EventStore for InProcessEventStore {
    async fn store_event(
        &self,
        stream_id: &str,
        event: &ServerSseMessage,
    ) -> Result<EventId, EventStoreError> {
        // Relaxed: this only needs to hand out distinct, increasing values,
        // not synchronize with anything else -- ordering among concurrent
        // stores is fixed by the `events` write lock below, not by the
        // counter fetch itself.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut events = self.events.write().await;
        events.push_back(StoredEvent {
            id,
            stream_id: stream_id.to_owned(),
            message: event.clone(),
        });
        while events.len() > MAX_BUFFERED_EVENTS {
            events.pop_front();
        }
        Ok(id.to_string())
    }

    async fn replay_events_after(
        &self,
        last_event_id: &str,
    ) -> Result<EventStream, EventStoreError> {
        let events = self.events.read().await;
        // An unparseable or aged-out ID isn't distinguishable from "the
        // client is too far behind to resume" -- both just mean nothing to
        // replay, so this degrades to an empty stream rather than an error
        // (see module doc).
        let Some(last_id) = last_event_id.parse::<u64>().ok() else {
            return Ok(Box::pin(futures::stream::empty()));
        };
        let Some(stream_id) = events
            .iter()
            .find(|stored| stored.id == last_id)
            .map(|stored| stored.stream_id.clone())
        else {
            return Ok(Box::pin(futures::stream::empty()));
        };
        let replay: Vec<ServerSseMessage> = events
            .iter()
            .filter(|stored| stored.stream_id == stream_id && stored.id > last_id)
            .map(|stored| stored.message.clone())
            .collect();
        Ok(Box::pin(futures::stream::iter(replay)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    // Content is irrelevant to every assertion here -- `store_event` never
    // looks inside the message, only `Clone`s it back out on replay -- so
    // the all-`None` default is a fine stand-in for a real SSE event.
    fn test_event() -> ServerSseMessage {
        ServerSseMessage::default()
    }

    #[tokio::test]
    async fn replay_returns_only_later_events_from_the_same_stream() {
        let store = InProcessEventStore::new();
        let a1 = store.store_event("stream-a", &test_event()).await.unwrap();
        let _b1 = store.store_event("stream-b", &test_event()).await.unwrap();
        let a2 = store.store_event("stream-a", &test_event()).await.unwrap();
        let _b2 = store.store_event("stream-b", &test_event()).await.unwrap();

        let replayed: Vec<_> = store
            .replay_events_after(&a1)
            .await
            .unwrap()
            .collect()
            .await;
        // Only stream-a's later event (a2), never stream-b's.
        assert_eq!(replayed.len(), 1);

        let replayed_from_a2: Vec<_> = store
            .replay_events_after(&a2)
            .await
            .unwrap()
            .collect()
            .await;
        assert!(replayed_from_a2.is_empty());
    }

    #[tokio::test]
    async fn unknown_or_unparseable_last_event_id_replays_as_empty_not_an_error() {
        let store = InProcessEventStore::new();
        store.store_event("stream-a", &test_event()).await.unwrap();

        let unknown: Vec<_> = store
            .replay_events_after("not-a-real-id")
            .await
            .unwrap()
            .collect()
            .await;
        assert!(unknown.is_empty());

        let never_issued: Vec<_> = store
            .replay_events_after("999999")
            .await
            .unwrap()
            .collect()
            .await;
        assert!(never_issued.is_empty());
    }

    #[tokio::test]
    async fn buffer_evicts_oldest_events_beyond_the_cap() {
        let store = InProcessEventStore::new();
        let first_id = store.store_event("stream-a", &test_event()).await.unwrap();
        for _ in 0..MAX_BUFFERED_EVENTS {
            store.store_event("stream-a", &test_event()).await.unwrap();
        }
        // The very first event aged out; replaying from it now looks
        // identical to an unknown ID -- empty, not an error.
        let replayed: Vec<_> = store
            .replay_events_after(&first_id)
            .await
            .unwrap()
            .collect()
            .await;
        assert!(replayed.is_empty());
    }
}
