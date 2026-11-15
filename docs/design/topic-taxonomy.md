# Topic taxonomy (design)

Status: Proposed, awaiting the owner's approval. No code yet.
Board: Mill Board #1188 (roadmap M4). Feeds #1311 (knowledge-hub distill) and
#1315 (semantic-zoom graph, under #764).

## Problem

Knowledge forms a topic hierarchy: Engineering → Systems Engineering → MBSE;
Software Engineering → SDLC, best practices (owner, 2026-11-06, on #764). The
graph's semantic zoom needs it to draw a few big topic stars before any
memory, and agents need it to search the knowledge hub by topic. Today a
memory has a free `category`, free tags and entity mention links. None of them
is hierarchical, and none says what a memory is *about*.

Requirements from the card: seed from embedding clusters, name clusters with
the local model (BaileyAi llama-swap), let one memory sit under several
topics, and add no memory columns (rows are bincode-positional, so a column
is a migration).

## What the repo already gives us

| Need | Existing piece | Fit |
|---|---|---|
| A named node with aliases and a stable id | `entity.rs`: id = hash of the normalised name, aliases union-merge, `Entities::rename` | good |
| Many-to-many memory ↔ node | `memory_entities` links, `link_memory_entity` | good, but a link means "mentions" today |
| Filter search to one node | `remind_me_search` `entity` filter | good, one level only |
| A mutable, evidenced, reversible attribute of an entity | ADR-0027 claims (`project.parent` etc.): supersedable fact memories, append-only history, compensating revert | good |
| Typed edges | `entity_relations` | **poor**: insert-or-ignore, immutable, no delete, so a topic could never move |
| Vectors | `vec_chunks` (per chunk, live model `qwen3-embedding:0.6b`, 1024-d) | good; a memory's vector is its normalised chunk mean |
| Grouping by cosine, naming by local model | `wiki_compile.rs` greedy `cluster`, `ChatCompletionsWriter` (ADR-0033) | reuse the adapter; the greedy cluster is too flat for a tree |
| Derived cache that is never the truth | `ann_index.rs` | the pattern for topic centroids |

## 1. Representation

**A topic is an entity named `topic:<Name>` whose shape lives in ADR-0027-style
claims. Membership is a `memory_entities` link.** No new table, no column, no
schema bump.

- **Name.** `TopicName` is a newtype that always renders with the `topic:`
  prefix, so the entity id is `hash("topic:mbse")`. That keeps topic
  membership apart from mention links: a memory that *mentions* MBSE links to
  the entity `mbse`; a memory *about* MBSE links to `topic:mbse`. Mention
  extraction and `remind_me_add` refuse entity names starting `topic:`, the
  way ADR-0027 refuses `project.` predicates. Two topics with the same leaf
  name are qualified by the user or the model (`topic:Testing (hardware)`).
- **Shape.** Claims with a closed predicate set, written and projected like
  projects:

  | predicate | object | cardinality |
  |---|---|---|
  | `topic.parent` | parent topic name, or `root` | one |
  | `topic.status` | `proposed` \| `active` \| `retired` | one |

  One parent per topic: a strict tree, which is what semantic zoom draws.
  Multi-membership is at the memory level, not the topic level. Re-parenting
  is a superseding claim with evidence, so moves have history and can be
  reverted (ADR-0027 §2), which immutable relations cannot give.
- **Projection.** `topic::tree(store) -> TopicTree` reads live claims and
  builds the typed tree (`Topic { entity, parent: Option<TopicId>, status,
  member_count }`). Cycles or a missing parent go to `invalid` with a reason,
  never dropped, as in ADR-0027 §3.
- **Membership.** One link per (memory, topic). Removing one needs a
  single-link `Entities::unlink(memory_id, entity_id)`; only
  `unlink_memory` (all links) exists today. It is a new store operation on
  the same table, not a schema change. Links record membership only for
  leaves and interior topics a memory was assigned to directly; ancestors are
  implied, never stored.

Rejected:

- **`parent_of` relations** (the card's first idea). Relations are immutable,
  so a topic could never be moved, merged out of a branch or un-made.
- **Tags** (`topic/mbse`). Tags already filter, but every assignment would
  rewrite the memory row (revision history, `updated_at`, sync churn), and
  bulk tag edits could silently drop topics.
- **A `topics` table.** Typed in storage, but a migration and a new record
  type, for a few hundred rows.

## 2. Seeding

A one-off CLI run, in three resumable steps. Nothing is written to the store
until a person applies the result.

1. **Cluster** (`rusty-remind-me topics seed`). Input: one vector per
   *refined* memory (fact, decision, preference, summary, wiki page, and later
   distilled hub items). Raw dialogs and captures are excluded: they match
   everything, and they inherit topics from the facts promoted out of them
   (provenance, ADR-0029), at read time.
   - Algorithm: recursive spherical k-means, hand-rolled (about 150 lines, no
     new dependency), deterministic init (farthest-first from the medoid), so
     a run is reproducible and testable on fixture vectors.
   - Shape: up to three levels. Level 1 k ≈ 10-15, then split any node over
     `leaf_max` (default 300) with k = clamp(n/150, 2, 8), down to level 3.
     Leaves end at roughly 50-300 memories.
   - Cost on 25k memories at 1024-d on the 2-core host: a level-1 iteration
     is 25k × 15 × 1024 ≈ 0.4 G multiply-adds, about half a second; the whole
     tree is under a minute. Vectors are about 100 MB in RAM (4 GB host); the
     run streams chunk rows and keeps only memory means.
   - Output: a snapshot file beside the store (`topics/seed-<run>.json`) with
     the embedding identity, parameters, the tree, member ids, and for each
     node its 12 members nearest the centroid. A changed embedding identity
     makes a snapshot unusable, as with the ANN index.
2. **Name** (`rusty-remind-me topics name --max-batches N`). Bottom-up: a
   leaf is named from its 12 nearest members (title or first 300 chars each);
   a parent is named from its children's names. One call per node through
   the existing `ChatCompletionsWriter` port with a topic prompt, answer
   `{name, description}`. About 200-300 nodes for 25k memories, so about two
   hours at ~30 s a call. Each name is written into the snapshot as it
   returns, so a stopped run resumes at the first unnamed node. Runs take the
   same one-run-at-a-time claim as wiki compile (ADR-0033 §6), widened to
   "the local model", so the two never fight over BaileyAi's single slot.
3. **Apply** (`rusty-remind-me topics apply <run> [--dry-run]`). Prints the
   named tree with counts; the owner edits names in the snapshot or the
   dashboard, then apply writes the topic entities, the `active` claims and
   the links in transactions per topic. Re-running apply on the same run is a
   no-op.

## 3. Assignment, rebalancing, curation

**New memories: at write time, nearest centroid.** `queries.rs` already embeds
every memory synchronously when it is added. Right after that, a refined
memory (not a dialog or capture) is linked to the closest leaf at cosine ≥ `REMIND_ME_TOPIC_MATCH` (default
from the seed's own leaf spread, overridable) and to any other leaf within
0.05 of the best, at most three. Below the floor it gets no topic and joins
the unsorted pool. This mirrors ADR-0033's page-matching rule. Cost: a dot
product against a few hundred centroids, microseconds. No embedder, no topic,
and a later sweep (`topics assign --pending`) catches it up.

Centroids are a derived cache (`topics/centroids.bin`, keyed by embedding
identity and link count), rebuilt from links and vectors when stale, never
the source of truth.

**Rebalancing produces proposals, never writes.** `topics rebalance` (CLI,
on demand, timer-able) computes:

- split: a leaf over `leaf_max` → k-means into 2-4 children, named by the
  model, proposed as `proposed` topics;
- new leaf: unsorted-pool clusters of 20+ memories;
- merge: a leaf under 10 members into its nearest sibling.

Proposals are confirmed or rejected like ADR-0027 project proposals (keyed,
a rejection is remembered). Moving members on a confirmed split re-links
them; nothing else moves.

**Human curation.** Rename is `Entities::rename` to a new `topic:` name plus
alias (old name stays findable). Merge reuses the entity merge from #567
(repoint links, retire the loser with a `topic.status = retired` claim).
Move is a new `topic.parent` claim. All three from the CLI first, the
dashboard Topics view later (#764).

## 4. Retrieval

- **Search.** One new property on `remind_me_search`: `topic`, by name or
  alias. It matches memories linked to that topic *or any descendant*, which
  is why it is not the existing `entity` filter (one entity, no closure).
  Same ADR-0030 group (`core`), no new tool. Raw dialogs match through the
  facts derived from them only when `include_raw` is set.
- **Browsing.** Reads go through what exists: `remind_me_entity` on a topic
  returns its parent, children and counts from the projection. The graph
  (#1315) gets one HTTP read, `GET /api/topics?parent=<id>` → children with
  member counts and their top members, which is exactly one zoom step.
- **No new MCP tool in v1.** Writes (apply, confirm, rename, merge, move) are
  CLI and dashboard. If agents need to curate later, one `remind_me_topics`
  tool in `pipeline`, like `remind_me_project`.
- **Brief and wiki: not in v1.** Candidate uses (the brief listing the
  project's top topics; wiki compile grouping pages by leaf topic) have no
  second caller yet. Revisit after the graph ships.

## 5. Evaluation

The #59 harness (`tests/retrieval_eval_test.rs`) gains:

- fixture rows with topic links and a small synthetic tree, including an
  ambiguous term under two branches ("testing" in hardware and in software);
- queries that carry `topic`, scored recall@5/@10 and MRR against the same
  query without it, with the lift printed and the filtered scores gated by
  `baseline.json`;
- `must_not` rows from the other branch, so a filter that leaks fails outright,
  and a check that a parent topic finds its descendants' members.

Clustering itself is unit-tested on hand-built vectors with known groups
(purity = 1, deterministic output). On the live node, `REMIND_ME_EVAL_LIVE=1`
prints the seed's purity against existing project tags as a sanity number;
it is not gated, because CI has no model.

## 6. Risks and cost

| Risk | Mitigation |
|---|---|
| A small local model names clusters badly | Owner reviews the tree before apply; rename is cheap and keeps an alias |
| Embedding model changes | Membership is links, so the tree survives; centroids and snapshots are keyed by embedding identity and rebuilt |
| Topic noise in mention-based features (entity lists, profile, graph) | The `topic:` namespace; entity listings can hide `topic:` entities by default |
| Over-assignment blurs filters | At most three topics, a floor, and the eval's `must_not` rows |
| Two hours on BaileyAi's single slot | One-off, batched, resumable, shares wiki compile's run claim |
| Tree churn confuses the graph | Only confirmed proposals change the tree; status `retired`, never deleted |

## 7. PR plan

1. **This doc** (draft PR). On approval it becomes ADR-0034.
2. **Topic core**: `TopicName`, `topic.*` claims, projection and `invalid`,
   guarded writes and the `topic:` refusal on generic paths, single-link
   unlink, `remind_me_entity` showing a topic's tree position. Unit tests.
3. **Seed, name, apply**: memory vectors, recursive k-means, snapshot,
   naming over the existing chat-completions port with resume, apply and
   dry run. Then the owner runs it on the node.
4. **Assign and search**: centroid cache, write-time assignment and
   `assign --pending`, the `topic` search property, eval fixture and
   baseline.
5. **Rebalance and curation**: proposals, confirm/reject, rename, merge (after
   #567), move, and `GET /api/topics` for #1315.

## Options and recommendation

- **A. Entities + claims + links (this doc).** No migration, history and
  revert for free, reuses the project pattern, the entity filter path and the
  wiki's model port. Cost: the tree's typing lives in one module, not in
  storage.
- **B. Entities + immutable `parent_of` relations.** Smallest build, but a
  topic can never move or be cleanly merged, which rebalancing needs.
- **C. A `topics` table.** Strongest typing; needs a migration (one-way since
  v33) and a new synced record type, for a few hundred rows.

**Recommendation: A.**
