SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;
SET default_tablespace = '';
SET default_table_access_method = heap;
CREATE TABLE public.entities (
    id text NOT NULL COLLATE pg_catalog."C",
    name text NOT NULL,
    kind text,
    aliases jsonb DEFAULT '[]'::jsonb NOT NULL,
    created_at text NOT NULL COLLATE pg_catalog."C",
    updated_at text NOT NULL COLLATE pg_catalog."C",
    node_id text,
    origin_node text
);
CREATE TABLE public.entity_relations (
    id text NOT NULL COLLATE pg_catalog."C",
    subject_entity_id text NOT NULL COLLATE pg_catalog."C",
    relation text NOT NULL,
    object_entity_id text NOT NULL COLLATE pg_catalog."C",
    created_at text NOT NULL COLLATE pg_catalog."C",
    updated_at text NOT NULL COLLATE pg_catalog."C",
    node_id text,
    origin_node text
);
CREATE TABLE public.memories (
    id text NOT NULL COLLATE pg_catalog."C",
    content text NOT NULL,
    category text DEFAULT 'general'::text NOT NULL,
    tags jsonb DEFAULT '[]'::jsonb NOT NULL,
    source text DEFAULT 'manual'::text NOT NULL,
    metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at text NOT NULL COLLATE pg_catalog."C",
    updated_at text NOT NULL COLLATE pg_catalog."C",
    capture_id text,
    node_id text,
    client text DEFAULT 'unknown'::text NOT NULL,
    accessed_at text COLLATE pg_catalog."C",
    access_count integer DEFAULT 0 NOT NULL,
    decay_rate double precision DEFAULT 0.1 NOT NULL,
    vitality double precision DEFAULT 1.0 NOT NULL,
    base_weight double precision DEFAULT 1.0 NOT NULL,
    status text DEFAULT 'active'::text NOT NULL,
    memory_type text DEFAULT 'unclassified'::text NOT NULL,
    source_capture_id text,
    subject text,
    predicate text,
    object text,
    superseded_by text,
    deleted_at text COLLATE pg_catalog."C",
    origin_node text,
    hub_seq bigint,
    sensitive boolean DEFAULT false NOT NULL,
    remind_at text COLLATE pg_catalog."C"
);
CREATE SEQUENCE public.memories_hub_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
CREATE TABLE public.memory_entities (
    memory_id text NOT NULL COLLATE pg_catalog."C",
    entity_id text NOT NULL COLLATE pg_catalog."C",
    created_at text NOT NULL COLLATE pg_catalog."C"
);
INSERT INTO public.entities VALUES ('e1', 'name of e1', 'person', '["Ada", "Lovelace"]', '2026-08-01T00:00:00+00:00', '2026-08-06T00:00:00+00:00', NULL, 'node-b');
INSERT INTO public.entities VALUES ('E2', 'name of E2', NULL, '[]', '2026-08-01T00:00:00+00:00', '2026-08-05T10:00:00+00:00', NULL, 'node-a');
INSERT INTO public.entity_relations VALUES ('r2', 'e1', 'knows', 'e2', '2026-08-05T10:00:00+00:00', '2026-08-05T10:00:00+00:00', NULL, 'node-a');
INSERT INTO public.entity_relations VALUES ('r1', 'e1', 'knows', 'e2', '2026-08-05T10:00:00+00:00', '2026-08-05T10:00:00+00:00', NULL, 'node-a');
INSERT INTO public.memories VALUES ('m1', 'content of m1', 'work', '["a", "m1"]', 'manual', '{"id": "m1"}', '2026-08-01T00:00:00+00:00', '2026-08-05T10:00:00+00:00', NULL, NULL, 'unknown', '2026-08-01T00:00:00+00:00', 0, 0.25, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, NULL, 'node-a', 1, true, NULL);
INSERT INTO public.memories VALUES ('m', 'content of m', 'general', '["a", "m"]', 'manual', '{"id": "m"}', '2026-08-01T00:00:00+00:00', '2026-08-05T10:00:00+00:00', NULL, NULL, 'unknown', '2026-08-01T00:00:00+00:00', 0, 0.25, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, NULL, 'node-b', 2, true, NULL);
INSERT INTO public.memories VALUES ('Z', 'content of Z', 'general', '["a", "Z"]', 'manual', '{"id": "Z"}', '2026-08-01T00:00:00+00:00', '2026-08-05T10:00:00+00:00', NULL, NULL, 'unknown', '2026-08-01T00:00:00+00:00', 0, 0.25, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, NULL, 'node-a', 3, false, NULL);
INSERT INTO public.memories VALUES ('cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 'content of cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 'work', '["a", "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"]', 'manual', '{"id": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}', '2026-08-01T00:00:00+00:00', '2026-08-05T10:00:00.500000+00:00', NULL, NULL, 'unknown', '2026-08-01T00:00:00+00:00', 0, 0.25, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, NULL, 'node-c', 4, false, NULL);
INSERT INTO public.memories VALUES ('m2', 'content of m2', 'work', '["a", "m2"]', 'manual', '{"id": "m2"}', '2026-08-01T00:00:00+00:00', '2026-08-06T00:00:00+00:00', NULL, NULL, 'unknown', '2026-08-03T00:00:00+00:00', 0, 0.25, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, NULL, 'node-a', 8, true, NULL);
INSERT INTO public.memories VALUES ('u1', 'content of u1', 'work', '["a", "u1"]', 'manual', '{"id": "u1"}', '2026-08-01T00:00:00+00:00', '2026-08-05T12:00:00+00:00', NULL, NULL, 'unknown', '2026-08-01T00:00:00+00:00', 0, 0.25, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 9, false, NULL);
INSERT INTO public.memories VALUES ('gone', 'x', 'general', '[]', 'manual', '{}', '2026-08-01T00:00:00+00:00', '2026-08-02T00:00:00+00:00', NULL, NULL, 'unknown', '2026-08-01T00:00:00+00:00', 0, 0.1, 1, 1, 'active', 'unclassified', NULL, NULL, NULL, NULL, NULL, '2026-08-02T00:00:00+00:00', 'node-a', 10, false, NULL);
INSERT INTO public.memory_entities VALUES ('m1', 'e1', '2026-08-05T10:00:00+00:00');
INSERT INTO public.memory_entities VALUES ('m', 'e1', '2026-08-05T10:00:00+00:00');
INSERT INTO public.memory_entities VALUES ('gone', 'e1', '2026-08-05T10:00:00+00:00');
INSERT INTO public.memory_entities VALUES ('nope', 'e2', '2026-08-05T09:00:00+00:00');
SELECT pg_catalog.setval('public.memories_hub_seq', 11, true);
ALTER TABLE ONLY public.entities
    ADD CONSTRAINT entities_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.entity_relations
    ADD CONSTRAINT entity_relations_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.memories
    ADD CONSTRAINT memories_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.memory_entities
    ADD CONSTRAINT memory_entities_pkey PRIMARY KEY (memory_id, entity_id);
CREATE INDEX idx_entities_updated_at_id ON public.entities USING btree (updated_at, id);
CREATE INDEX idx_entity_relations_created_at_id ON public.entity_relations USING btree (created_at, id);
CREATE INDEX idx_links_created_at ON public.memory_entities USING btree (created_at);
CREATE INDEX idx_memories_hub_seq ON public.memories USING btree (hub_seq);
CREATE INDEX idx_memories_updated_at_id ON public.memories USING btree (updated_at, id);
