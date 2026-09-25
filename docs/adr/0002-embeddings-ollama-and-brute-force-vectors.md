# ADR-0002: Embeddings via Ollama's HTTP API; vectors stored and scanned in plain SQL, not `sqlite-vec`

Status: Accepted
Date: 2026-07-29

## Context

`#49` is the largest remaining gap: this crate's search is FTS5 keyword
matching only. `MemorySearchResult.vec_score` exists and is always `None`,
so the RRF fusion already wired up in `retrieval.rs` fuses one ranked list
with nothing. `vec_chunks` and `embedding_meta` are in the generated schema,
both unused. The issue is explicit that the embedding backend must be
decided and recorded here, before writing any of it, because the options —
a Rust ONNX binding, a subprocess, or a remote API — differ hugely in build
weight, offline behaviour, and whether the crate still compiles without the
dependency.

Two separate questions turned out to be entangled here, and needed
separating: what generates a vector, and what stores/searches one.

### What generates a vector

The reference supports **two** backends, not one — `embeddings.py`'s
`_get_embedder()` returns either:

- `_Embedder`: ONNX Runtime, running `sentence-transformers/all-MiniLM-L6-v2`
  (or any configured model) in-process, downloaded from HuggingFace Hub on
  first use.
- `OllamaEmbedder`: a `POST /api/embed` call to a local Ollama daemon, with
  `{"model": ..., "input": [...]}` in, `{"embeddings": [[...], ...]}` out.

Feasibility of the ONNX route was checked, not assumed: adding `ort` (the
Rust ONNX Runtime binding) to a scratch crate and forcing real usage (not
just a dependency listed but unreferenced) showed it downloads and links a
prebuilt ONNX Runtime successfully through this environment's network setup.
So the ONNX path is not blocked on tooling. It is blocked on everything
downstream of tooling: a tokenizer (`tokenizers` crate, itself a real
dependency), a HuggingFace Hub download-and-cache pipeline, and a model file
this crate would need to fetch, verify, and keep working across an
`all-MiniLM-L6-v2` model bundle's own format assumptions (input names,
pooling strategy, output shape) that the reference's Python code currently
hard-codes.

The Ollama route needs none of that. It is an HTTP POST and a JSON body —
implementable with the same hand-rolled protocol-client approach already
established for the webhook endpoint (`#56`) and the HTTP API (`#48`), with
zero new compile-time dependencies. It is also not an invented shortcut: it
is one of the reference's own two supported, documented backends, not a
lesser substitute for the "real" one.

### What stores and searches a vector

The reference's actual vector store is `memories_vec`, a `sqlite-vec` `vec0`
virtual table — `CREATE VIRTUAL TABLE memories_vec USING
vec0(embedding float[{dim}])`. `vec_chunks` (already in this crate's
generated schema) is *only* the rowid map back to `memory_rowid`/`chunk_ix`;
the actual float bytes live in `memories_vec`, behind the `sqlite-vec`
loadable extension, probed per-connection and treated as optional exactly
like the embedder itself (`sqlite_vec.load(db)` wrapped in
try/except — the reference's own "exact brute-force scan" still queries
`memories_vec` and returns nothing if that extension didn't load).

`sqlite-vec` is a native, loadable SQLite extension. `rusqlite`'s `bundled`
feature (already in use here, and the reason `#24` needed a hand-registered
scalar function for `exp`/`sqrt`) does not include it, and there is no
Rust-native `vec0` implementation to depend on instead — only a path that
means fetching and linking a second native library, per-platform, the same
category of dependency this crate has consistently avoided (`#53`'s ADR
declined exactly this shape of problem for MemPalace's vector segment).

## Decision

**Embedding backend: Ollama's `POST /api/embed`, via a hand-rolled HTTP
client over `std::net::TcpStream`.** Configured by `REMIND_ME_EMBEDDING_BACKEND`
(only `"ollama"` enables it — matching the reference's `EMBEDDING_BACKEND`
switch, minus the ONNX arm), `REMIND_ME_OLLAMA_URL` (default
`http://localhost:11434`), `REMIND_ME_OLLAMA_EMBED_MODEL` (default
`nomic-embed-text`), and `REMIND_ME_EMBEDDING_DIM` (default `384` — matches
the reference's own default, which assumes the ONNX model this crate does
not implement; anyone turning Ollama on for real should set this to their
model's actual dimension, exactly as the reference requires). Unset or
unreachable, this degrades to FTS-only search — the same posture `#55`/`#56`
established for "no secret configured" and "no store found": a feature that
is off by default rather than a hollow stub pretending to be on.

ONNX-in-process remains available as a future addition behind the same
`Embedder` trait, should it ever be wanted — the feasibility check above
means that decision is not blocked on tooling if someone picks it up later.
It is out of scope here because Ollama alone fully satisfies the graceful-
degradation requirement with a fraction of the engineering risk, and this
crate has consistently favored the smaller, more auditable dependency at
every prior fork in the road (`#48`'s synchronous server over async
frameworks, `#52`/`#53`'s direct-SQL reads over vendor client libraries).

**Vector storage: a new table, `vec_embeddings(vec_rowid INTEGER PRIMARY
KEY, embedding BLOB NOT NULL)`, not `sqlite-vec`'s `vec0`.** `vec_chunks`
keeps its generated-schema role as the rowid map — untouched, not extended
with a new column, because `schema_tables.sql` is generated verbatim from
`remind_me`'s `sqlite_master` and is not this crate's file to hand-edit
(`migrations.rs`'s own stated rule). `vec_embeddings` is new, created by this
crate's own code after the generated schema is applied — analogous to, but
not literally, the reference's `memories_vec`. A database shared with
`remind_me` is unaffected: it does not know about `vec_embeddings` and
would not look for it.

Vectors are stored as raw float32 bytes with dimension inferred from
`len(bytes) / 4`, matching the reference's own convention exactly — this is
what keeps the column backend-agnostic across a 384/768/1024-dimensional
model without a schema change.

**Semantic search is a brute-force cosine-similarity scan over
`vec_embeddings`, in Rust, not SQL `MATCH`.** Filtered first by
`memory_rowid`'s category/tag/`superseded_by`/`deleted_at` predicates
pushed into ordinary SQL — the same "filter before limit" principle
(`DI-03`) already established for FTS search — before the per-vector
dot-product comparison runs at all. This produces identical retrieval
quality to the reference's own exact-scan path: cosine similarity is cosine
similarity, whether SQLite's `vec0 MATCH` operator computes it or this
crate's own loop does. It is a different mechanism for an identical
observable result, not a lesser one.

**ANN (the optional HNSW index above `ANN_MIN_CHUNKS`), reranking, and
query expansion are explicitly out of scope for this decision.** The issue's
own acceptance criteria does not require them — only that `vec_score` is
real, `remind_me_reindex` exists, and everything degrades gracefully without
the backend. The issue itself calls ANN "explicitly optional," consulted
only above a chunk-count threshold most vaults will never reach; the brute
force path is the one every deployment actually exercises below that
threshold, in the reference as much as here. Building ANN, a reranker, and
query expansion without a working brute-force baseline to build them on top
of would be exactly backwards.

## Alternatives considered

**ONNX Runtime in-process (the reference's default backend).** Rejected for
now, not on feasibility (verified working) but on scope: it requires a
tokenizer dependency, a HuggingFace-Hub download-and-cache pipeline, and
hard-coding one specific model's I/O contract, none of which the Ollama
route needs. Revisitable behind the same trait without disturbing anything
built here.

**Shelling out to a Python subprocess running the reference's own
`embeddings.py`.** Rejected for the same reason `#53`'s ADR rejected it for
MemPalace: it reintroduces a Python runtime dependency into a binary whose
entire point is not needing one, for a feature (Ollama) that doesn't need
it either.

**Loading `sqlite-vec` as a runtime extension.** Rejected: no mature
Rust-native wrapper, and self-bundling a second native shared library per
platform is a maintenance burden with no corresponding gain in retrieval
quality over a brute-force scan at the scale this crate actually needs to
serve below `ANN_MIN_CHUNKS`.

## Consequences

- Turning semantic search on is one environment variable
  (`REMIND_ME_EMBEDDING_BACKEND=ollama`) plus a running Ollama daemon — no
  new build-time dependency, no change to the binary when it's off.
- A database's `vec_embeddings` table is specific to this crate; `remind_me`
  opening the same file would not read or write it, and this crate does not
  read or write `memories_vec` either. The two vector stores do not
  interoperate — only the memory rows themselves do, as before.
- The brute-force scan is O(chunks) per query. This is the reference's own
  behavior below `ANN_MIN_CHUNKS` (5000 chunks); above it, the reference
  reaches for HNSW. This crate has no ANN path yet, so a very large vault
  pays the full linear scan unconditionally. That is a real, known
  limitation, not an oversight — see "ANN" above.
- Should ONNX-in-process, ANN, reranking, or query expansion ever be wanted,
  each is its own decision and its own ADR, not a quiet extension of this
  one.

## Addendum: embedding-model versioning and auto-clear (#96)

The reference (`67570ce`) records which model produced `memories_vec`'s
vectors in `embedding_meta` (`key`/`value`/`updated_at`, keyed on `model`,
`dim`, `backend`), checks it against the configured model at every startup
(`_reconcile_embedding_meta`), and on a mismatch clears
`memories_vec`/`vec_chunks` — recreating `memories_vec` at the new
dimension, since a `vec0` virtual table's column type bakes the dimension
into its `CREATE VIRTUAL TABLE` statement — plus its on-disk ANN index, so
every memory falls back to the existing "missing embeddings" path instead of
silently serving results computed against the wrong embedding space.
`embedding_meta` is only updated once vectors are actually rewritten, not
merely inferred from config, so the mismatch stays flagged across every
connection until a real reindex happens.

This crate's schema already carries the same `embedding_meta` table
(schema-parity boilerplate, generated verbatim like the rest of
`schema_tables.sql`) but nothing read or wrote it until now. The mechanism
above is ported with two adaptations, both consequences of decisions this
ADR already made:

- **No table recreation on a dimension change.** `vec_embeddings` (this
  crate's stand-in for `memories_vec`, decided above) is a plain `BLOB`
  column with the dimension inferred from `len(bytes) / 4` at read time, not
  baked into the column type the way `vec0 USING vec0(embedding
  float[{dim}])` is. A dimension change therefore needs no `DROP`/`CREATE`;
  clearing rows is the whole story. `crate::vectors::reconcile_embedding_meta`
  deletes from `vec_embeddings` before `vec_chunks` (the reverse of the
  reference's order) because this crate's `vec_embeddings.vec_rowid` has a
  real foreign key onto `vec_chunks.vec_rowid` — the reference has no such
  constraint between `memories_vec` and `vec_chunks`, they are only joined by
  convention.
- **No ANN index to invalidate.** This ADR scoped ANN, reranking, and query
  expansion out entirely (see "ANN" above) — brute-force cosine scan is the
  only search path this crate has, so there is no on-disk index that could
  go stale in the first place.

Identity is the same triple the reference uses — backend, model, dimension —
but sourced from `Embedder::identity()` (a new trait method) rather than
three separate config globals: this crate's only real backend is Ollama, and
tying the recorded identity to the actual embedder instance that wrote a
batch of vectors (rather than to whatever the environment happens to say
right now) is what keeps `mark_embedding_meta_current` accurate under a
test's injected fake embedder, exactly as much as under the real one. The
startup check itself (`db::schema::initialize_schema`) still reads
`embedder::resolve_embedder()` — config only, no network probe — matching
the reference's own check. When no backend is configured at all
(`REMIND_ME_EMBEDDING_BACKEND` unset — the default, unlike the reference's
always-on ONNX default), the check is skipped outright: nothing was written
by this process either, so there is nothing meaningful to compare against.
