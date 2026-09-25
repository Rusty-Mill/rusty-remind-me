-- GENERATED from remind_me's schema. Do not hand-edit.
-- Regenerate with: python3 scripts/regenerate_schema.py --reference <path>

CREATE TABLE IF NOT EXISTS analytics_snapshots (
            id               INTEGER PRIMARY KEY,
            captured_at      TEXT NOT NULL,
            total_memories   INTEGER NOT NULL,
            vitality_buckets TEXT NOT NULL,
            category_counts  TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS chat_imports (
            import_id   TEXT PRIMARY KEY,
            filename    TEXT NOT NULL,
            hash        TEXT NOT NULL,
            imported_at TEXT NOT NULL,
            stats       TEXT NOT NULL DEFAULT '{}'
        );

CREATE TABLE IF NOT EXISTS dbs_imports (
            dbs_source   TEXT NOT NULL,
            external_id  TEXT NOT NULL,
            memory_id    TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            imported_at  TEXT NOT NULL,
            PRIMARY KEY (dbs_source, external_id)
        );

CREATE TABLE IF NOT EXISTS embedding_meta (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS entities (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            kind        TEXT DEFAULT NULL,
            aliases     TEXT NOT NULL DEFAULT '[]',  -- JSON array
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            node_id     TEXT DEFAULT NULL
        );

CREATE TABLE IF NOT EXISTS entity_relations (
            id                TEXT PRIMARY KEY,
            subject_entity_id TEXT NOT NULL,
            relation          TEXT NOT NULL,
            object_entity_id  TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL,
            node_id           TEXT DEFAULT NULL
        );

CREATE TABLE IF NOT EXISTS memories (
            id          TEXT PRIMARY KEY,
            content     TEXT NOT NULL,
            category    TEXT NOT NULL DEFAULT 'general',
            tags        TEXT NOT NULL DEFAULT '[]',  -- JSON array
            source      TEXT NOT NULL DEFAULT 'manual',
            metadata    TEXT NOT NULL DEFAULT '{}',  -- JSON object
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        , capture_id TEXT DEFAULT NULL, node_id TEXT DEFAULT NULL, client TEXT NOT NULL DEFAULT 'unknown', accessed_at TEXT DEFAULT NULL, access_count INTEGER NOT NULL DEFAULT 0, decay_rate REAL NOT NULL DEFAULT 0.1, vitality REAL NOT NULL DEFAULT 1.0, base_weight REAL NOT NULL DEFAULT 1.0, status TEXT NOT NULL DEFAULT 'active', memory_type TEXT NOT NULL DEFAULT 'unclassified', source_capture_id TEXT DEFAULT NULL, subject TEXT DEFAULT NULL, predicate TEXT DEFAULT NULL, object TEXT DEFAULT NULL, superseded_by TEXT DEFAULT NULL, doc_id TEXT DEFAULT NULL, chunk_index INTEGER DEFAULT NULL, deleted_at TEXT DEFAULT NULL, remind_at TEXT DEFAULT NULL, sensitive INTEGER NOT NULL DEFAULT 0);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
            content, category, tags,
            content='memories',
            content_rowid='rowid'
        );

CREATE TABLE IF NOT EXISTS memory_associations (
            memory_id_a TEXT NOT NULL,
            memory_id_b TEXT NOT NULL,
            weight      INTEGER NOT NULL DEFAULT 1,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (memory_id_a, memory_id_b)
        );

CREATE TABLE IF NOT EXISTS memory_entities (
            memory_id   TEXT NOT NULL,
            entity_id   TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            PRIMARY KEY (memory_id, entity_id)
        );

CREATE TABLE IF NOT EXISTS memory_feedback (
            id           TEXT PRIMARY KEY,
            memory_id    TEXT NOT NULL,
            query        TEXT NOT NULL,
            query_tokens TEXT NOT NULL,
            signal       TEXT NOT NULL CHECK (signal IN ('helpful', 'unhelpful')),
            magnitude    REAL NOT NULL,
            created_at   TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS memory_revisions (
            id              INTEGER PRIMARY KEY,
            memory_id       TEXT NOT NULL,
            content         TEXT,
            category        TEXT,
            tags            TEXT,
            metadata        TEXT,
            edited_at       TEXT NOT NULL,
            revision_reason TEXT DEFAULT NULL
        , sensitive INTEGER DEFAULT NULL);

CREATE TABLE IF NOT EXISTS memory_tags (
            memory_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            tag        TEXT NOT NULL,
            PRIMARY KEY (memory_id, tag)
        );

CREATE TABLE IF NOT EXISTS mempalace_imports (
            drawer_id   TEXT PRIMARY KEY,
            memory_id   TEXT NOT NULL,
            imported_at TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS reminder_deliveries (
            id           INTEGER PRIMARY KEY,
            memory_id    TEXT NOT NULL,
            remind_at    TEXT NOT NULL,
            delivered_at TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS saved_search_seen_memories (
            saved_search_id TEXT NOT NULL,
            memory_id       TEXT NOT NULL,
            first_seen_at   TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS saved_searches (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            query      TEXT NOT NULL,
            filters    TEXT NOT NULL,
            watch      INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS sync_flags (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS sync_log (
            remote_id   TEXT NOT NULL,
            last_pull   TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00',
            last_push   TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00', last_pull_id TEXT NOT NULL DEFAULT '', last_attempt_at TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00', last_push_at TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00', last_pull_at TEXT NOT NULL DEFAULT '1970-01-01T00:00:00+00:00', last_pull_seq INTEGER NOT NULL DEFAULT -1,
            PRIMARY KEY (remote_id)
        );

CREATE TABLE IF NOT EXISTS sync_outbox (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id   TEXT NOT NULL,
            operation   TEXT NOT NULL,
            payload     TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            sent_at     TEXT DEFAULT ''
        );

CREATE TABLE IF NOT EXISTS sync_sends (
            remote_id  TEXT NOT NULL,
            outbox_id  INTEGER NOT NULL,
            sent_at    TEXT NOT NULL,
            PRIMARY KEY (remote_id, outbox_id)
        );

CREATE TABLE IF NOT EXISTS vec_chunks (
               vec_rowid    INTEGER PRIMARY KEY,
               memory_rowid INTEGER NOT NULL,
               chunk_ix     INTEGER NOT NULL
           );

CREATE VIRTUAL TABLE IF NOT EXISTS wiki_fts USING fts5(
            title, content,
            content='wiki_pages',
            content_rowid='rowid'
        );

CREATE TABLE IF NOT EXISTS wiki_links (
            src_slug   TEXT NOT NULL,
            dst_slug   TEXT NOT NULL,
            dst_title  TEXT NOT NULL,
            PRIMARY KEY (src_slug, dst_slug)
        );

CREATE TABLE IF NOT EXISTS wiki_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

CREATE TABLE IF NOT EXISTS wiki_pages (
            slug        TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            content     TEXT NOT NULL,
            summary     TEXT NOT NULL DEFAULT '',
            mtime       REAL NOT NULL DEFAULT 0,
            updated_at  TEXT NOT NULL
        );
