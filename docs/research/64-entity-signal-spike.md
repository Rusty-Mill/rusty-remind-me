# Spike: entity overlap as a ranking signal (Mill Board #64)

Status: **measured 2026-11-26**, fixture half in a cloud session, real-query
half on Nano's host against a copy of the live store.
Verdict under D92: **drop**. The fixture lift came from synthetic queries; on
the #1563 real-query set the best non-zero weight lifts entity-query MRR by
only 0.002 and lowers their recall@10. The weight stays 0 by default.

The code described below is not on `main`. It stays unmerged on branch
`claude/project-thread-q37cqo-64` (commit 7441752), with both ignored
runners, so the spike can be rerun from there.

## Question

Does ranking already-retrieved candidates by how many of the query's
entities they are linked to improve search enough to keep?

## Keep rule

Nano, 2026-11-26 (pack D92): keep if the best non-zero weight lifts recall@10
or MRR by at least 2 points on entity queries, and no other query kind or
strategy drops more than 0.02. Otherwise drop. The rule has two halves: the
fixture and the off-repo #1563 real-query set.

## What was built (pack D87)

- `RrfWeights::w_entity`, default 0.0, read from `REMIND_ME_RRF_W_ENTITY` and
  scaled by the strategy presets the way `w_idf` is (multiplier 1.0 in every
  preset). At 0 nothing is looked up and every score is unchanged.
- `entity::resolve_query_entities`: every run of 1 to 3 query words (split on
  whitespace, punctuation trimmed from word ends, a possessive `'s` dropped),
  compared with each entity's name and aliases in `normalize_entity_name`
  form. Topic entities are skipped. One pass over all entities per search.
- The signal: for each candidate in the keyword and semantic union, the number
  of query entities it is linked to (`Memories::linked_ids`, the link index).
  Rank mode adds `w_entity / (k + rank)`, where tied overlaps share a rank;
  score mode adds `w_entity * overlap / max overlap`. A candidate with no
  overlap gets 0, so a query that names no entity ranks exactly as before. No
  new hits are added. Expansion sections are unchanged. The share is added to
  `score` only: no new response field.

## Method

- **Harness.** The #59 harness (`tests/retrieval_eval/shared.rs`), its gated
  modes only (keyword, hashed), HyDE and rerank off, all four strategies,
  weights 0, 0.5 and 1.0. Runner: `tests/entity_signal_spike_test.rs`, an
  ignored test (`cargo test -p remind_me_core --release --test
  entity_signal_spike_test -- --ignored --nocapture`).
- **Clock.** Fixture ages are relative to the run, and the `+dates` modes
  (which move with the weekday) are not run, so the ranking does not depend
  on the date. The gated test, run on its own in a second process, printed
  the same weight-0 numbers as the spike.
- **Two stores.** *Base*: the #59 fixture (108 rows) and its 82 queries,
  unchanged. At weight 0 it gives the gated test's numbers. *Synthetic*: the
  #59 fixture plus 37 new rows (17 entity-linked across six entities with
  aliases, 20 unlinked distractors that share the topic words), scored with
  20 new queries: 17 `entity` queries whose answer is linked to the entity
  the query names, and 3 `entity-adversarial` queries that name an entity
  whose linked rows are *not* the answer. The committed baseline and golden
  set are untouched.
- **Entity queries for the rule.** Chosen before the run: the 12 base
  `entity` queries plus all 20 synthetic ones, adversarial included, pooled
  over the four strategies (128 query-runs per mode). "Other kinds" are the
  8 non-entity base kinds, plus each strategy's total over the base set.
- **Wins and losses.** A query wins when its recall@10 plus reciprocal rank
  rises against weight 0, and loses when it falls.
- **Latency.** Wall time per search, 5 repeats of every query under every
  strategy in both modes and both stores (4,080 searches per weight), on a
  release build. The store has 145 rows and 9 entities at most.

## Results

### Entity queries (pooled, all strategies)

| mode | weight | recall@5 | recall@10 | MRR | MRR change |
|---|---|---|---|---|---|
| keyword | 0 | 0.896 | 0.896 | 0.632 | |
| keyword | 0.5 | 0.896 | 0.896 | 0.791 | +0.159 |
| keyword | 1.0 | 0.896 | 0.896 | 0.791 | +0.159 |
| hashed | 0 | 0.865 | 0.958 | 0.612 | |
| hashed | 0.5 | 0.943 | 0.958 | 0.862 | +0.249 |
| hashed | 1.0 | 0.943 | 0.958 | 0.841 | +0.228 |

Recall@10 did not move. The signal only reorders candidates already in the
pool, and on this fixture no relevant candidate sat just past rank 10.

### By query set (MRR change against weight 0, range over the four strategies)

| set | n | keyword 0.5 | keyword 1.0 | hashed 0.5 | hashed 1.0 | wins/losses per strategy |
|---|---|---|---|---|---|---|
| base `entity` | 12 | 0 | 0 | 0 | 0 | 0/0 |
| synthetic `entity` | 17 | +0.366 to +0.416 | same | +0.509 to +0.567 | same | 12 or 13 / 0 |
| synthetic `entity-adversarial` | 3 | -0.417 to -0.556 | same | -0.389 | -0.611 | 0/2 or 0/3 |

Hashed recall@5 on synthetic `entity` rose 0.137 to 0.176 at both weights;
keyword recall@5 did not move.

### Other kinds and strategies

Every non-entity base kind (exact, paraphrase, temporal, reversed, hidden,
dated, dormant, topic), in both modes and under all four strategies, moved by
exactly 0.000 on every metric at both weights, with no wins or losses. So did
each strategy's total. Base weight-0 totals (equal to the gated run):

| | auto | balanced | keyword_favored | semantic_favored |
|---|---|---|---|---|
| keyword r@10 / MRR | 0.904 / 0.777 | 0.904 / 0.770 | 0.904 / 0.778 | 0.904 / 0.766 |
| hashed r@10 / MRR | 0.921 / 0.807 | 0.921 / 0.806 | 0.921 / 0.789 | 0.921 / 0.807 |

These are 0.000 to 0.006 above the committed `baseline.json` (for example
keyword/auto MRR 0.777 against 0.7705). The same gap shows on the gated test
at weight 0 and is not caused by this change. The baseline was not
re-blessed.

### Latency (per search, microseconds, release build)

| weight | p50 | p95 | p50 added | p95 added |
|---|---|---|---|---|
| 0 | 217 | 467 | | |
| 0.5 | 226 | 472 | +9 | +5 |
| 1.0 | 226 | 454 | +9 | -13 |

The added cost is within noise on this store. Resolution scans every entity
once per search, so the cost grows with entity count. A store with thousands
of entities should be timed before the weight is turned on.

## Verdict under D92

| weight | entity lift (best of recall@10, MRR) | worst drop elsewhere | passes |
|---|---|---|---|
| 0.5 | +0.204 MRR | 0.000 | yes |
| 1.0 | +0.194 MRR | 0.000 | yes |

**Fixture half passes; the real-query half fails (below), so the verdict is drop.** The best
non-zero weight is 0.5. Adversarial queries count in the entity pool, not as
an "other kind" in the drop check (D93), and their losses are listed below.
On keep, a draft PR opens with this code; on drop, a docs-only PR carries this
write-up and the code stays unmerged on its branch.

Caveats that limit this result:

- **All of the lift comes from the synthetic set**, which was written for
  this spike. The base fixture's `entity` queries do not move: its entities
  are slugs (`dock-controller`, `juniper-okafor`), which a query's words
  never match, and the two that do resolve (`pedalpath`, `harbor`) already
  rank their answer first.
- **The signal hurts when the named entity is not what the user wants.**
  Every adversarial query lost MRR, by 0.39 to 0.61. They count in the pooled
  entity number (pre-registered), which still clears the bar. They do not
  count as an "other kind" for the drop check. How often real queries look
  like this is what the real-query half has to show.
- Resolution is exact: no fuzzy, hyphen or plural matching. "dock
  controller" does not find `dock-controller`.

## Real queries (#1563 set)

Measured 2026-11-26 on BaileyAi, against a copy of the newest remind-me
backup (2026-11-26), with the store's own embedder (ollama
`qwen3-embedding:0.6b`, 1024 dims, matching the stored vectors). The query
set and raw outputs stay on that host (D27); only aggregates are here.

- **Path.** `search_with_expansions`, the `remind_me_search` path: top 10,
  no token budget (0), compact detail, HyDE and rerank off, default strategy,
  `REMIND_ME_DAEMON=0`. Runner: `tests/entity_signal_real_queries_test.rs`,
  ignored, reading the store, query file and output path from the
  environment.
- **Entity pool.** A query is in the pool when
  `entity::resolve_query_entities` resolves at least one entity from it on
  the copy: **37 of 50**. That takes 12 of the 13 labelled `entity` queries,
  15 of 22 `exact` and 10 of 15 `paraphrase`. The rest keep their kind.
- **Search writes.** The search path records accesses and co-retrieval
  pairs, and those writes move later rankings: a first pass on one shared
  copy changed the top 10 of every query, non-entity ones included. So each
  weight starts from a fresh copy, and only the first search of each query
  is scored. A second weight-0 run on a fresh copy matched the first exactly
  (0 of 50 rankings differed), so the deltas below are the signal, not drift.

| group | n | weight | recall@10 | MRR | recall@10 change | MRR change | wins/losses |
|---|---|---|---|---|---|---|---|
| entity pool | 37 | 0 | 0.890 | 0.826 | | | |
| entity pool | 37 | 0.5 | 0.872 | 0.829 | -0.018 | +0.002 | 5/4 |
| entity pool | 37 | 1.0 | 0.863 | 0.821 | -0.027 | -0.005 | 5/4 |
| exact (unresolved) | 7 | 0, 0.5, 1.0 | 0.952 | 0.929 | 0.000 | 0.000 | 0/0 |
| paraphrase (unresolved) | 5 | 0, 0.5, 1.0 | 0.733 | 0.800 | 0.000 | 0.000 | 0/0 |
| entity (unresolved) | 1 | 0, 0.5, 1.0 | 1.000 | 1.000 | 0.000 | 0.000 | 0/0 |
| all | 50 | 0 | 0.885 | 0.841 | | | |
| all | 50 | 0.5 | 0.872 | 0.843 | -0.013 | +0.002 | 5/4 |
| all | 50 | 1.0 | 0.865 | 0.838 | -0.020 | -0.004 | 5/4 |

Latency per search (5 searches per query, 250 per weight, release build), in
milliseconds: weight 0 p50 561 / p95 663, 0.5 565 / 664, 1.0 561 / 660; the
weight-0 rerun gave 559 / 672. The added cost is within run-to-run noise. Most
of the time is the query embedding and the store's size, not the signal.

Recall@10 falls even though the signal adds no hits: it reorders the whole
keyword and semantic union before the cut at 10, so a relevant row with no
entity link can drop out of the top 10 behind linked rows. Over a third of
the entity pool (15 of 37) are queries labelled `exact` or `paraphrase`,
where the words that resolve to an entity are incidental, the real-world
case of the fixture's adversarial queries.

**Drop under D92:** the best non-zero weight (0.5) lifts entity-pool MRR by
0.002 and recall@10 by -0.018, short of the 0.02 lift, while no other kind
moves. The fixture half passed only on queries this spike wrote.

## Open questions

- Should `entity-adversarial` queries count as an "other kind" in D92's drop
  check? Counting them, the fixture result is drop: they lose more than
  0.02.
- If the signal is kept, should the result carry its share as its own field
  (for example `entity_score`), as the other signals do? That changes the
  response, which is public API (D3).
