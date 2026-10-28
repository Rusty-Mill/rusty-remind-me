-- The Python hub's schema: 11 columns, TIMESTAMPTZ timestamps.
CREATE TABLE memories (
    id         TEXT PRIMARY KEY,
    content    TEXT NOT NULL,
    category   TEXT NOT NULL DEFAULT 'general',
    tags       JSONB NOT NULL DEFAULT '[]',
    source     TEXT NOT NULL DEFAULT 'manual',
    metadata   JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    capture_id TEXT,
    node_id    TEXT,
    client     TEXT NOT NULL DEFAULT 'unknown'
);
INSERT INTO memories (id, content, tags, created_at, updated_at)
VALUES ('legacy-1', 'from the old hub', '["a"]',
        '2026-08-05 10:00:00+00', '2026-08-05 11:30:00+00'),
       ('legacy-2', 'also old', '[]',
        '2026-08-04 08:00:00+00', '2026-08-04 09:00:00.500000+00');
