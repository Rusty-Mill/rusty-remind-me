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
INSERT INTO "entities" VALUES('e1','name of e1','person','["Ada","Lovelace"]','2026-08-01T00:00:00+00:00','2026-08-06T00:00:00+00:00',NULL,'node-b');
INSERT INTO "entities" VALUES('E2','name of E2',NULL,'[]','2026-08-01T00:00:00+00:00','2026-08-05T10:00:00+00:00',NULL,'node-a');
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
INSERT INTO "entity_relations" VALUES('r2','e1','knows','e2','2026-08-05T10:00:00+00:00','2026-08-05T10:00:00+00:00',NULL,'node-a');
INSERT INTO "entity_relations" VALUES('r1','e1','knows','e2','2026-08-05T10:00:00+00:00','2026-08-05T10:00:00+00:00',NULL,'node-a');
CREATE TABLE hub_meta (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    high_water INTEGER NOT NULL
);
INSERT INTO "hub_meta" VALUES(1,8);
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
INSERT INTO "memories" VALUES('m1','content of m1','work','["a","m1"]','manual','{"id":"m1"}','2026-08-01T00:00:00+00:00','2026-08-05T10:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.25,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,'node-a',1,1,NULL);
INSERT INTO "memories" VALUES('m','content of m','general','["a","m"]','manual','{"id":"m"}','2026-08-01T00:00:00+00:00','2026-08-05T10:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.25,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,'node-b',2,1,NULL);
INSERT INTO "memories" VALUES('Z','content of Z','general','["a","Z"]','manual','{"id":"Z"}','2026-08-01T00:00:00+00:00','2026-08-05T10:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.25,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,'node-a',3,0,NULL);
INSERT INTO "memories" VALUES('cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc','content of cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc','work','["a","cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"]','manual','{"id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}','2026-08-01T00:00:00+00:00','2026-08-05T10:00:00.500000+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.25,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,'node-c',4,0,NULL);
INSERT INTO "memories" VALUES('m2','content of m2','work','["a","m2"]','manual','{"id":"m2"}','2026-08-01T00:00:00+00:00','2026-08-06T00:00:00+00:00',NULL,NULL,'unknown','2026-08-03T00:00:00+00:00',0,0.25,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,'node-a',6,1,NULL);
INSERT INTO "memories" VALUES('u1','content of u1','work','["a","u1"]','manual','{"id":"u1"}','2026-08-01T00:00:00+00:00','2026-08-05T12:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.25,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,NULL,NULL,7,0,NULL);
INSERT INTO "memories" VALUES('gone','x','general','[]','manual','{}','2026-08-01T00:00:00+00:00','2026-08-02T00:00:00+00:00',NULL,NULL,'unknown','2026-08-01T00:00:00+00:00',0,0.1,1.0,1.0,'active','unclassified',NULL,NULL,NULL,NULL,NULL,'2026-08-02T00:00:00+00:00','node-a',8,0,NULL);
CREATE TABLE memory_entities (
    memory_id  TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (memory_id, entity_id)
);
INSERT INTO "memory_entities" VALUES('m1','e1','2026-08-05T10:00:00+00:00');
INSERT INTO "memory_entities" VALUES('m','e1','2026-08-05T10:00:00+00:00');
INSERT INTO "memory_entities" VALUES('gone','e1','2026-08-05T10:00:00+00:00');
INSERT INTO "memory_entities" VALUES('nope','e2','2026-08-05T09:00:00+00:00');
CREATE INDEX idx_entities_updated_at_id ON entities (updated_at, id);
CREATE INDEX idx_entity_relations_created_at_id
    ON entity_relations (created_at, id);
CREATE INDEX idx_links_created_at ON memory_entities (created_at);
CREATE INDEX idx_memories_hub_seq ON memories (hub_seq);
CREATE INDEX idx_memories_updated_at_id ON memories (updated_at, id);
COMMIT;
