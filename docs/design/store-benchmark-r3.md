# Store benchmark spec (Millwright R3, card #1618)

Status: **signed off by Nano, 2026-11-21** ("Approve as written", decision
card `cmsg_012spbR8vYb6jWVUuV6ALzxuHfQhCeUcyFwPpCFLQdtBQ9`): gates,
targets, the choosing rule and the raw-bodies rule below are fixed. The run
waits for the storage port to finish (#1617, ADR-0035 step 6).

## Question

Which store should hold remind-me from R4 on: the current
`rusty_multimodal_db` engine, SQLite, or Attest? The winner also hosts the
artifact module (#1190) and the transcript store (#528), so it must hold raw
pages and full dialogs on the 4 GB remindme host, which the engine cannot
today: it keeps every record and its full-text index in RAM
(`dbs-modularity-audit.md`, "All records in RAM").

Decision record: D2 and D14 in `docs/handoff/DECISIONS.md` (refactor first,
store at benchmark; SQLite acceptable if it wins); Attest added as a
candidate by Nano on 2026-11-16 (`mem_efa1025a87974ad483c47b32b0d24c42`).
Drift: rusty_mill `docs/projects/millwright/ROADMAP.md` R3 still names
"Ledger's store"; Attest replaced it.

## Candidates

| Candidate | Adapter | Search indexes |
|---|---|---|
| Engine | the existing ADR-0035 port impls on `EngineTables` | its own in-RAM full-text index, `vectors.rs` |
| SQLite | new port impls over `rusqlite` (already a dependency, bundled) | FTS5; vectors as BLOBs scored in process, as the engine does |
| Attest | new port impls over Attest's append-only log | none built in (exact reads only); the adapter adds the same in-process full-text and vector indexes, and that cost counts against it |

All three run behind the same port traits and must pass the same conformance
suites before any number counts. Updates and deletes on Attest are written as
superseding records; that growth, and the cost of rebuilding its indexes
from the log at open and of reclaiming space, is measured, not hidden.

**Raw bodies.** Every adapter, the engine included, may keep raw
page and dialog bodies outside its RAM-resident tables, in the reusable blob
store (audit seam 4), and only metadata in the tables. That follows the
audit's option A ("raw bodies stay off the RAM-resident tables whichever
store wins") and keeps G5 a fair test rather than ruling the engine out from
the start.

**Conformance must include** (G1): dialog rows cannot be rewritten in place
(D13, #1616), and readers get consistent results while the single writer
writes.

## Host

Not the remindme host, and never the live store. BaileyAi, with the benchmark
process inside a cgroup that matches remindme: `systemd-run --scope -p
MemoryMax=4G -p CPUQuota=200%`. Release build, one candidate per process, the
page cache dropped before each cold-open run, and no local-model job running
alongside (BaileyAi has one model slot; D16, D37).

## Datasets

| Id | What | Source |
|---|---|---|
| D1 | The real store: about 25,000 memories, their entities, references and stored 1024-dim vectors | copy of the newest remindme backup (`manual-*.engine`, about 500 MB), loaded into each candidate through the port |
| D2 | D1 plus synthetic memories to 75,000 (3x growth headroom) | `examples/store_bench.rs` generator; their vectors are seeded random unit vectors, so the embedder is not touched |
| D3 | D1 plus raw artifacts: 50,000 pages of 10 to 30 KB markdown and every captured dialog in full, about 1.5 GB of text | #763's clipped folder if it is ready, otherwise synthetic pages sized to that distribution |

Query vectors are embedded once and saved, so the embedder is never in the
timed path. Relevance is checked on the CI golden set
(`retrieval_eval_test.rs`, 82 queries) and the 50 real #1563 queries.

## Workloads

1. **Search:** keyword, semantic and hybrid, top 10, 500 queries each.
2. **Vitality:** score every memory once (the scheduler's pass), and the
   vitality read inside each search.
3. **Entity graph:** one-hop and two-hop neighbours of 500 entities.
4. **Writes:** single `add` with indexing; `update`; `delete`.
5. **Bulk ingest:** load D3's pages and dialogs, with searches running
   alongside.
6. **Open:** cold open (process start to first query answered).
7. **Crash:** `kill -9` at 200 random points during writes and ingest, then
   reopen and check.
8. **Concurrency:** four reader threads searching while one writer adds.
9. **Backup:** take a backup of D3 and restore it to a fresh directory,
   verified (D36).

## Thresholds

A candidate that misses any **gate** is out. Store-side time only; the MCP
round trip is not measured.

| # | Gate | Threshold | Why this number |
|---|---|---|---|
| G1 | Conformance | 100% of port conformance suites pass | same behaviour or no comparison |
| G2 | Relevance | recall@10 and MRR on both query sets no more than 2 points below the engine | borrows the 2-point tolerance Nano set for #1563 (D23) |
| G3 | Steady memory | resident at most 1.0 GB with D3 loaded and idle | leaves about 3 GB for the connector, UI, OS and the release build that OOM'd the host on 2026-11-06 |
| G4 | Peak memory | resident at most 1.5 GB at any point in workloads 1 to 7 | same budget, with room for ingest bursts |
| G5 | Raw text stays off RAM | resident with D3 at most 1.25x resident with D1 | the reason for R3: 1.5 GB of raw text must not become 1.5 GB of RAM |
| G6 | Crash safety | 200 of 200 kills: reopens cleanly, loses no acknowledged write, `verify` (or `PRAGMA integrity_check`) clean | remind-me is the memory every agent leans on |
| G7 | Cold open | at most 3 s on D1, at most 5 s on D3 | ADR-0023's existing 3 s bar; clients wait 60 s for a daemon |
| G8 | Backup | a verified restore of D3 succeeds | D36: backups are a precondition |

Scored, not gating (reported for every candidate that passes the gates):

| Metric | Target | Dataset |
|---|---|---|
| Keyword search p95 | at most 50 ms | D1 and D2 |
| Semantic search p95 | at most 100 ms | D1 and D2 |
| Hybrid search p95 | at most 150 ms | D1 and D2 |
| Hybrid p95 during bulk ingest | at most 2x its idle p95 | D3 |
| Full vitality pass | at most 10 s | D2 |
| Entity one-hop / two-hop p95 | at most 20 ms / 100 ms | D1 |
| Single add p95 | at most 50 ms | D1 |
| Bulk ingest of D3 | at most 30 min | D3 |
| Disk footprint | at most 2x the raw text | D3 |
| Search p95 with 4 readers and 1 writer | at most 2x its idle p95 | D1 |
| Backup and restore time | reported | D3 |

## Choosing

1. Drop every candidate that misses a gate.
2. Among the rest, the one that meets the most scored targets wins (this
   also covers the case where none meets them all).
3. Ties go to the one with less storage code for us to own (adapter plus
   engine lines), because D6/D14 say Nano would trade "pure Rust" for not
   maintaining storage code. This rule leans toward SQLite, and Nano
   approved it knowing that.

The write-up recommends; Nano picks the store. Migrating the live store is a
separate decision and card (R4).

## Attest readiness

Reported as found, not forced. As of 2026-11-21 Attest is local,
single-writer, exact-read only (no full-text or vector search), with in-memory
indexes rebuilt from the log (`rebuild_indexes`), and it is being hardened by
the measurement-records workload and daemon work first (Nano, 2026-11-16). If
its adapter cannot pass G1 when the port is ready, the write-up says "did not
run" with the reason instead of a number.

## Deliverables of the run

`docs/research/store-benchmark-r3.md`: method, aggregate results per
dataset, and the recommendation with its tradeoffs. The raw JSON lines and the
#1563 query set hold real text and live ids, so they stay out of the repo, in
the project files under `remind-me/1618-store-bench/` (D38).
The harness extends `examples/store_bench.rs`.
