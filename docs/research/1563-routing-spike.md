# Spike: hierarchical embedding routing (Mill Board #1563)

Status: **measured 2026-11-18**, on BaileyAi against a copy of the remindme
store backup of 2026-11-17 (embedder `qwen3-embedding:0.6b`, 1024 dims, the
same model as the stored vectors). Verdict: **no-go**.

## Question

Can search find the right region of memory through a tree of cluster
centroids (names for precision, vectors for recall) and hand a model less
text for the same recall? This is the fixed-compression baseline: LLM-free
centroids over the frozen embedding model. A learned router is out of scope;
this spike also produces its first training data.

## Go rule

Nano, 2026-11-18 (pack D23): go if, at some beam width,

- routed recall@10 is within 2 points of flat recall@10,
- routed search reaches that recall with at least 30% fewer tokens than flat@10,
- routed p95 latency is at most twice flat p95.

## Method

- **Corpus.** Every seedable memory with vectors (not dialogs, skeletons,
  sensitive memories or project/topic records): the same set `topics seed`
  clusters, one unit vector per memory (the mean of its chunks).
- **Tree.** `topic_seed::cluster_tree` (recursive spherical k-means, 12 top
  clusters, leaves of at most 300) or a saved seed run (`--run`). A node's
  centroid is the unit mean of every member below it.
- **Routed search.** Beam search: score the top level, keep the best `beam`
  nodes, open each interior one into its children, keep the best `beam`
  again, until only leaves are kept; rank those leaves' members exactly.
- **Flat search.** Exact cosine over the whole corpus, same vectors and same
  query embedding, so the difference is routing alone. The production hybrid
  ranker (keyword + vector + RRF) is not the baseline (D25).
- **Eval set.** 50 real questions about recent work (22 exact, 15 paraphrase,
  13 entity), each labelled with the memory ids that answer it. Claude
  labelled them from remind-me on 2026-11-18; Nano spot-checks 10. It holds
  live ids and stays out of the repo (D27): project file
  `remind-me/1563-routing/queries.jsonl`.
- **Tokens.** `retrieval::estimated_content_tokens` (bytes / 4) over the full
  content of the returned memories (D26). "Tokens to match" is the smallest
  routed k whose recall reaches flat recall@10 less 2 points, priced at that k.
- **Latency.** In-process ranking time only (both paths read vectors from
  memory; embedding the query costs the same for both and is left out).
- **Descent log.** `--log` writes one JSON line per routed query per beam:
  the nodes kept at each step with scores and labels, the leaves ranked,
  vectors scored, the top ten ids and whether each was relevant, plus the run
  id and embedding model (D28).

## What to expect

Routing over the same vectors can only change a ranking by *excluding*
memories outside the chosen leaves. So:

- recall can only fall at a fixed k unless the excluded memories were
  distractors ranked above relevant ones;
- the token win, if any, comes from that exclusion lifting relevant memories
  into a smaller k;
- the sure win is compute: vectors scored per query drops from the corpus
  size to roughly `beam × leaf size` plus the centroids.

If recall holds but tokens do not drop by 30%, the honest reading is that
routing is a scale optimisation (an alternative to the ANN index), not a
context saving, and the context saving needs the summary layer (label and
summary per node handed to the model instead of leaves).

## Results

| beam | recall@10 | flat recall@10 | tokens to match (k) | flat tokens@10 | vectors scored | p95 / flat p95 | go |
|---|---|---|---|---|---|---|---|
| 1 | 0.402 | 0.965 | not reached | 3346 | 249 / 8974 | 537 µs / 10076 µs (0.05×) | no |
| 2 | 0.535 | 0.965 | not reached | 3346 | 451 / 8974 | 637 µs / 10076 µs (0.06×) | no |
| 4 | 0.641 | 0.965 | not reached | 3346 | 867 / 8974 | 1129 µs / 10076 µs (0.11×) | no |
| 8 | 0.758 | 0.965 | not reached | 3346 | 1674 / 8974 | 2119 µs / 10076 µs (0.21×) | no |

Vectors scored and tokens are per-query means. Routed tokens@10 were
3944 / 3706 / 3760 / 3742 for beams 1 / 2 / 4 / 8.

**No-go.** At no beam width does routed recall@10 come within 2 points of
flat (best 0.758 at beam 8 against 0.965), so no routed k reaches the match
target and the token rule cannot be met; routed search does not hand the
model less text either, since its tokens@10 are above flat's. Only the
latency rule holds: routing scores 3-19% of the vectors and its p95 is
0.05-0.21× flat. That matches the "What to expect" reading: over the same
frozen vectors, centroid routing is a compute/scale optimisation that costs
recall, not a context saving. Run shape: corpus 8,974 memories, an
in-memory tree (no `--run`) of 92 nodes and 71 leaves at depth 3, 33 of the
50 queries measured; 17 were skipped because none of their relevant ids is
in the seedable corpus of this backup, and 48 relevant ids are missing in
all (newer than the 2026-11-17 backup, or not seedable). Unrouted: 0.

## Running it

On the remindme host, from a build of this branch:

```sh
rusty-remind-me routing eval queries.jsonl --beam 1,2,4,8 --log descents.jsonl > report.json
```

The command reads the store only (through the running daemon when there is
one) and needs the configured embedder to answer. To route by a named tree
instead of an in-memory clustering, run `topics seed` first and pass its run
id with `--run`.
