use crate::db::migrations;
use crate::vitality;
use rusqlite::{Connection, Result};

pub use crate::db::migrations::SCHEMA_VERSION;

/// Open-time setup: connection pragmas, the SQL helper functions, then schema
/// creation and reconciliation.
///
/// See [`migrations`] for how the schema is defined — it is generated from
/// `remind_me` rather than written here.
pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        -- Two independent processes (the `remote` MCP server and the `api`
        -- dashboard, per docs' serve-mcp-rust.ps1 launcher) commonly cold-open
        -- the same database within moments of each other. On a database in
        -- the hundreds-of-MB range, one side's open-time checks can hold the
        -- write lock past a short timeout, which surfaced as both processes
        -- crashing with SqliteFailure(DatabaseBusy) instead of one simply
        -- waiting its turn. 5s wasn't enough headroom; 30s comfortably covers
        -- a slow cold open without masking a genuinely stuck lock.
        PRAGMA busy_timeout=30000;
        PRAGMA foreign_keys=ON;
        ",
    )?;

    // Scalar functions live on the connection, not in the file, so every
    // connection has to register them before running a query that uses one.
    vitality::register_sql_functions(conn)?;

    migrations::apply(conn)?;

    // Not part of the generated schema: this crate's own vector storage,
    // added after the generated tables exist. See
    // docs/adr/0002-embeddings-ollama-and-brute-force-vectors.md for why it
    // is a plain table rather than `sqlite-vec`'s `vec0`.
    crate::vectors::ensure_schema(conn)?;

    // Also not part of the generated schema: raw-transcript retention (#212).
    // Created even with retention off, so the read path never has to tolerate
    // a missing table. See `archive.rs` for why this cannot be a column on
    // `chat_imports`.
    crate::archive::ensure_schema(conn)?;

    // Nor is the refinement ladder's provenance table (#208). Same reasoning:
    // `schema_tables.sql` is generated verbatim, so a promoted artifact's
    // links live in a table this crate owns outright.
    crate::promotion::ensure_schema(conn)?;

    // Embedding-model versioning (#96): detect a changed
    // REMIND_ME_EMBEDDING_BACKEND/OLLAMA_EMBED_MODEL/EMBEDDING_DIM at every
    // open and clear now-invalid vectors -- the same "check at startup"
    // timing the reference uses. `resolve_embedder` is config-only (no
    // network probe), matching what the reference's own check reads. `None`
    // means nothing is configured to embed with, so there is nothing to
    // compare against and nothing was written by this process either.
    if let Some(embedder) = crate::embedder::resolve_embedder() {
        crate::vectors::reconcile_embedding_meta(conn, &embedder.identity())?;
    }

    // Every outbox trigger (memories and the graph tables alike) is gated on
    // sync_flags.sync_enabled -- align it with the current configuration on
    // every open, exactly like the reference does, before anything else
    // touches sync_outbox.
    crate::sync::reconcile_sync_enabled_flag(conn)?;

    // The outbox triggers fire on every write while the gate above is on, but
    // nothing here drains them. Applying the reference's own retention rule
    // on open keeps that from growing without bound.
    crate::sync::prune_outbox(conn)?;

    Ok(())
}
