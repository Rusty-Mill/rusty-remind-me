BEGIN;
CREATE TABLE entities (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT,
    aliases     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    node_id     TEXT,
    origin_node TEXT
);
CREATE TABLE entity_relations (
    id                TEXT PRIMARY KEY,
    subject_entity_id TEXT NOT NULL,
    relation          TEXT NOT NULL,
    object_entity_id  TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    node_id           TEXT,
    origin_node       TEXT
);
CREATE TABLE hub_meta (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    high_water INTEGER NOT NULL
);
INSERT INTO "hub_meta" VALUES(1,2);
CREATE TABLE memories (
    id                TEXT PRIMARY KEY,
    content           TEXT NOT NULL,
    category          TEXT NOT NULL DEFAULT 'general',
    tags              TEXT NOT NULL DEFAULT '[]',
    source            TEXT NOT NULL DEFAULT 'manual',
    metadata          TEXT NOT NULL DEFAULT '{}',
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    capture_id        TEXT,
    node_id           TEXT,
    client            TEXT NOT NULL DEFAULT 'unknown',
    accessed_at       TEXT,
    access_count      INTEGER NOT NULL DEFAULT 0,
    decay_rate        REAL NOT NULL DEFAULT 0.1,
    vitality          REAL NOT NULL DEFAULT 1.0,
    base_weight       REAL NOT NULL DEFAULT 1.0,
    status            TEXT NOT NULL DEFAULT 'active',
    memory_type       TEXT NOT NULL DEFAULT 'unclassified',
    source_capture_id TEXT,
    subject           TEXT,
    predicate         TEXT,
    "object"          TEXT,
    superseded_by     TEXT,
    deleted_at        TEXT,
    origin_node       TEXT,
    hub_seq           INTEGER,
    sensitive         INTEGER NOT NULL DEFAULT 0,
    remind_at         TEXT
);
INSERT INTO "memories" VALUES('ok','content of ok','general','[]','manual','{}','2026-08-01T00:00:00+00:00','2026-08-02T00:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.1,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,NULL,1,0,NULL);
INSERT INTO "memories" VALUES('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx','content of xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx','general','[]','manual','{}','2026-08-01T00:00:00+00:00','2026-08-02T00:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.1,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,NULL,2,0,NULL);
CREATE TABLE memory_entities (
    memory_id  TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (memory_id, entity_id)
);
INSERT INTO "memory_entities" VALUES('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx','e1','2026-08-02T00:00:00+00:00');
CREATE INDEX idx_entities_updated_at_id ON entities (updated_at, id);
CREATE INDEX idx_entity_relations_created_at_id
    ON entity_relations (created_at, id);
CREATE INDEX idx_links_created_at ON memory_entities (created_at);
CREATE INDEX idx_memories_hub_seq ON memories (hub_seq);
CREATE INDEX idx_memories_updated_at_id ON memories (updated_at, id);
COMMIT;
