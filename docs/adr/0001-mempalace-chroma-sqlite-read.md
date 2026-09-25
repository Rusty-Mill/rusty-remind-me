# ADR-0001: Read MemPalace's ChromaDB store via its SQLite backing file, never its vector index

Status: Accepted
Date: 2026-07-29

## Context

`#53` asks for `remind_me_import_mempalace`: a bulk pull of MemPalace drawers,
mirroring the reference's `mempalace_import.py`. The reference reads
MemPalace's persistent ChromaDB store directly — `chromadb.PersistentClient`,
read-only — because MemPalace's own MCP tools expose drawers one at a time,
which does not scale to a wing holding tens of thousands.

This is the one import source in the parity backlog with a genuinely hard
dependency: Rust has no ChromaDB client of comparable maturity, and the issue
is explicit that the decision belongs in an ADR rather than a commit message,
with "decline the tool" listed as a legitimate outcome if direct reading turns
out not to be feasible.

Three options were on the table:

1. Read Chroma's on-disk store directly with plain SQL, the same way `#52`'s
   `dbs` importer reads a foreign SQLite schema.
2. Shell out to a small Python helper (using the real `chromadb` package) and
   parse its output.
3. Decline the tool and close the issue.

The deciding question was not "can Rust drive ChromaDB" — it can't, not
without (2) — but "does the reference's own read path ever touch a vector at
all." It doesn't. `pull_mempalace` calls `collection.get(..., include=
["documents", "metadatas"])` — `embeddings` is conspicuously absent from that
list. The reference itself never asks Chroma to reconstruct or compare a
vector for this feature; it only ever wants the document text and the
key/value metadata attached to each drawer. That reframes the question from
"read a vector database" to "read the plain-text half of one," which is a much
smaller ask.

### What was actually verified, not assumed

`chromadb.PersistentClient` keeps everything for a local store in one file:
`{persist_directory}/chroma.sqlite3` (`chromadb/db/impl/sqlite.py`). Its schema
is defined by numbered, one-way SQL migrations shipped inside the package
(`chromadb/migrations/{sysdb,metadb}/*.sql`) — not a private, undocumented
binary format. The tables that matter:

```
collections(id TEXT PK, name TEXT UNIQUE, dimension INT, ...)
segments(id TEXT PK, type TEXT, scope TEXT, collection TEXT FK)
embeddings(id INTEGER PK, segment_id TEXT, embedding_id TEXT, seq_id, created_at)
embedding_metadata(id INTEGER FK->embeddings.id, key TEXT,
                   string_value TEXT, int_value INT, float_value REAL, bool_value INT)
```

A collection has (at least) two segments: one `scope = 'VECTOR'` (HNSW index,
its own binary files under a per-segment directory) and one
`scope = 'METADATA'` (backed entirely by the tables above). A drawer's document
text is not a separate column — Chroma stores it as an `embedding_metadata` row
under the reserved key `chroma:document` (`chromadb/api/types.py:
META_KEY_CHROMA_DOCUMENT`), alongside whatever user metadata (`wing`, `room`,
...) was set on the drawer, as ordinary rows in the same table. All ids
(`collections.id`, `segments.id`, `segments.collection`) are plain hyphenated
UUID text (`uuid_to_db` is just `str(uuid)`), not blobs — the migration's
`TEXT` column types are literal, not a formality.

This was checked against `chromadb==0.5.0` — the reference's own minimum pin —
and `chromadb==1.5.9`, the latest available at the time of writing. The
`embeddings`/`embedding_metadata`/`collections`/`segments` shape and the
`chroma:document` convention are byte-for-byte identical across that entire
range; migrations only ever add columns or indices, never restructure these
tables. That is not proof it will never change, but it is verified evidence
across the full version span this crate has to support, not a guess from
memory.

## Decision

Read `{MEMPALACE_PATH}/chroma.sqlite3` directly with `rusqlite`, opened
`SQLITE_OPEN_READ_ONLY`, exactly the posture `#52`'s `dbs` importer already
established for a foreign, someone-else's-backup SQLite file:

1. `SELECT id FROM collections WHERE name = 'mempalace_drawers'`
2. `SELECT id FROM segments WHERE collection = ? AND scope = 'METADATA'`
3. Walk `embeddings` rows for that segment, and for each, its
   `embedding_metadata` rows, pulling out `chroma:document` (the drawer's
   text) and `wing`/`room` (for filtering and tagging).
4. Apply the wing/room filter and `limit`/`offset` paging in this crate's own
   code — the reference lets Chroma's query planner do this server-side, but
   the shape being read (a flat key/value table) makes it just as simple to
   filter locally, and it avoids depending on whatever internal query-planning
   API Chroma exposes.

**The vector segment is never opened.** No HNSW index, no per-segment binary
files, no embedding math, at any point. That is not an optimization — it is
the entire reason this is tractable at all in a language with no ChromaDB
client.

`remind_me_import_mempalace` reads `REMIND_ME_MEMPALACE_PATH` for the store
location (default `~/.mempalace/palace`, matching the reference's
`MEMPALACE_PATH`), not a caller-supplied path — this is operator
configuration, like the folder watcher's directories, not a per-call
argument, so it does not go through the import-roots containment check
(`SE-02`) that a caller-supplied path would.

## Alternatives considered

**Shell out to a Python subprocess running the real `chromadb` package.**
Rejected. This reintroduces a Python runtime dependency into a binary whose
entire point is not needing one, for a feature that — per the investigation
above — needs nothing Python-specific once the vector segment is off the
table. It would also be strictly more fragile than a direct SQL read: a
subprocess boundary adds its own serialization, error-mapping, and
availability concerns on top of the schema-stability question, for no
compensating benefit.

**Decline the tool.** Rejected, but only because the investigation changed the
premise. Before checking what `include=[...]` actually requested, "Rust has no
ChromaDB client" reads as a hard stop. After checking, the honest framing is
narrower: Rust has no HNSW-reading client, and this feature was never going to
ask for one.

## Consequences

- No new dependency: `rusqlite` is already present everywhere else in this
  crate.
- This is coupled to Chroma's internal (not publicly documented, though
  stable-in-practice) local-persistence schema rather than its public Python
  API. If a future Chroma major version restructures `embeddings`/
  `embedding_metadata` or drops the `chroma:document` convention, this import
  path breaks without a compile-time signal — the same risk `#52`'s `dbs`
  importer already accepted for a foreign schema, here traded against
  verified stability across the full `0.5.0`–`1.5.9` range rather than a
  single snapshot.
- MemPalace drawer content is still opaque prose from this crate's point of
  view — no decoding of its AAAK dialect is introduced, matching the
  reference (`mempalace_import.py`'s own docstring: "designed to be read
  as-is").
- Should this ever need to widen (e.g. also reading vector data), that is a
  new ADR, not an extension of this one — reading the vector segment is a
  different, harder problem this decision explicitly does not attempt.
