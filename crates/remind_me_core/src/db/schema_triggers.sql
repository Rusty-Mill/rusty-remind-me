-- GENERATED from remind_me's schema. Do not hand-edit.
-- Regenerate with: python3 scripts/regenerate_schema.py --reference <path>

CREATE TRIGGER IF NOT EXISTS entities_outbox_ai
        AFTER INSERT ON entities
        WHEN COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'
        BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (NEW.id, 'insert', json_object('record_type', 'entity', 'id', NEW.id, 'name', NEW.name, 'kind', NEW.kind, 'aliases', NEW.aliases, 'created_at', NEW.created_at, 'updated_at', NEW.updated_at, 'node_id', NEW.node_id), strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00');
        END;

CREATE TRIGGER IF NOT EXISTS entities_outbox_au
        AFTER UPDATE ON entities
        WHEN COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'
        BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (NEW.id, 'update', json_object('record_type', 'entity', 'id', NEW.id, 'name', NEW.name, 'kind', NEW.kind, 'aliases', NEW.aliases, 'created_at', NEW.created_at, 'updated_at', NEW.updated_at, 'node_id', NEW.node_id), strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00');
        END;

CREATE TRIGGER IF NOT EXISTS entity_relations_outbox_ai
        AFTER INSERT ON entity_relations
        WHEN COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'
        BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (NEW.id, 'insert', json_object('record_type', 'entity_relation', 'id', NEW.id, 'subject_entity_id', NEW.subject_entity_id, 'relation', NEW.relation, 'object_entity_id', NEW.object_entity_id, 'created_at', NEW.created_at, 'updated_at', NEW.updated_at, 'node_id', NEW.node_id), strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00');
        END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
            VALUES ('delete', old.rowid, old.content, old.category, old.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
            INSERT INTO memories_fts(rowid, content, category, tags)
            VALUES (new.rowid, new.content, new.category, new.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
            VALUES ('delete', old.rowid, old.content, old.category, old.tags);
            INSERT INTO memories_fts(rowid, content, category, tags)
            VALUES (new.rowid, new.content, new.category, new.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_outbox_ai
        AFTER INSERT ON memories
        WHEN COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'
        BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (NEW.id, 'insert', json_object('id', NEW.id, 'content', NEW.content, 'category', NEW.category, 'tags', NEW.tags, 'source', NEW.source, 'metadata', NEW.metadata, 'created_at', NEW.created_at, 'updated_at', NEW.updated_at, 'capture_id', NEW.capture_id, 'node_id', NEW.node_id, 'client', NEW.client, 'accessed_at', NEW.accessed_at, 'access_count', NEW.access_count, 'decay_rate', NEW.decay_rate, 'vitality', NEW.vitality, 'base_weight', NEW.base_weight, 'status', NEW.status, 'memory_type', NEW.memory_type, 'source_capture_id', NEW.source_capture_id, 'subject', NEW.subject, 'predicate', NEW.predicate, 'object', NEW.object, 'superseded_by', NEW.superseded_by, 'doc_id', NEW.doc_id, 'chunk_index', NEW.chunk_index, 'deleted_at', NEW.deleted_at, 'remind_at', NEW.remind_at, 'sensitive', NEW.sensitive), strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00');
        END;

CREATE TRIGGER IF NOT EXISTS memories_outbox_au
        AFTER UPDATE ON memories
        WHEN COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'
             AND NEW.updated_at IS NOT OLD.updated_at
        BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (NEW.id, 'update', json_object('id', NEW.id, 'content', NEW.content, 'category', NEW.category, 'tags', NEW.tags, 'source', NEW.source, 'metadata', NEW.metadata, 'created_at', NEW.created_at, 'updated_at', NEW.updated_at, 'capture_id', NEW.capture_id, 'node_id', NEW.node_id, 'client', NEW.client, 'accessed_at', NEW.accessed_at, 'access_count', NEW.access_count, 'decay_rate', NEW.decay_rate, 'vitality', NEW.vitality, 'base_weight', NEW.base_weight, 'status', NEW.status, 'memory_type', NEW.memory_type, 'source_capture_id', NEW.source_capture_id, 'subject', NEW.subject, 'predicate', NEW.predicate, 'object', NEW.object, 'superseded_by', NEW.superseded_by, 'doc_id', NEW.doc_id, 'chunk_index', NEW.chunk_index, 'deleted_at', NEW.deleted_at, 'remind_at', NEW.remind_at, 'sensitive', NEW.sensitive), strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00');
        END;

CREATE TRIGGER IF NOT EXISTS memories_tags_ad AFTER DELETE ON memories
        BEGIN
            DELETE FROM memory_tags WHERE memory_id = OLD.id;
        END;

CREATE TRIGGER IF NOT EXISTS memories_tags_ai AFTER INSERT ON memories
        BEGIN
            INSERT OR IGNORE INTO memory_tags (memory_id, tag)
            SELECT NEW.id, je.value
              FROM json_each(NEW.tags) AS je
             WHERE typeof(je.value) = 'text'
               AND json_valid(NEW.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_tags_au AFTER UPDATE OF tags ON memories
        BEGIN
            DELETE FROM memory_tags WHERE memory_id = OLD.id;
            INSERT OR IGNORE INTO memory_tags (memory_id, tag)
            SELECT NEW.id, je.value
              FROM json_each(NEW.tags) AS je
             WHERE typeof(je.value) = 'text'
               AND json_valid(NEW.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memory_entities_outbox_ai
        AFTER INSERT ON memory_entities
        WHEN COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'
        BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (NEW.memory_id, 'insert', json_object('record_type', 'memory_entity', 'id', NEW.memory_id || '|' || NEW.entity_id, 'memory_id', NEW.memory_id, 'entity_id', NEW.entity_id, 'created_at', NEW.created_at), strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00');
        END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_ad AFTER DELETE ON wiki_pages BEGIN
            INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
            VALUES ('delete', old.rowid, old.title, old.content);
        END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_ai AFTER INSERT ON wiki_pages BEGIN
            INSERT INTO wiki_fts(rowid, title, content)
            VALUES (new.rowid, new.title, new.content);
        END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_au AFTER UPDATE ON wiki_pages BEGIN
            INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
            VALUES ('delete', old.rowid, old.title, old.content);
            INSERT INTO wiki_fts(rowid, title, content)
            VALUES (new.rowid, new.title, new.content);
        END;
