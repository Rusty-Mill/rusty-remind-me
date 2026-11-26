# R3 store benchmark: scoring run results (#1618)

Spec: `docs/design/store-benchmark-r3.md` (D40, D41). This page reports the
method and aggregate results only. The raw JSON lines, the #1563 query set
and its vectors hold real text and live ids, so they stay off the repo (D39).

**Result: the § Choosing rule names no store from this run.** Attest
misses G3 and G7 as observed; its G6 crash run started but did not
complete (no valid G6 result), and its G2 did not run. The G4 figures
here are each process's observed whole-run `VmHWM` (its lifetime
high-water, the D1 load included), reported as a diagnostic. They do not
establish the signed G4 verdict for workloads 1 to 7, so this page does not
use them to say any candidate passes or misses G4. Without a G4 verdict,
§ Choosing step 1 cannot be completed for the engine or SQLite. The scored
tables are below for the record. Nano's choice is in § Decision; migrating
the live store (R4) is a separate decision.

## Run

| | |
|---|---|
| Code | `65671958e3604f176c6b5a150a5617e591f69909`, the same build for all three candidates (`--locked`, release). It is befc347 plus #420 (connector credential; no store, bench or lockfile change), #423 (adds the `sha256` dependency the Attest bench crate was missing; build-only) and 6567195 (D1 loader streams stored rows so the D1 load fits in 4 GB). |
| `R3_GIT_SHA` | the sha above. The crash and relevance reports carry it. The bench reports have no `git_sha` field; their provenance is the run log's start line and their run directories. |
| Host | BaileyAi, never the remindme host or the live store. Each command ran as root, alone, under `systemd-run --scope -p MemoryMax=4G -p CPUQuota=200%`, with `R3_REQUIRE_CGROUP=1` and `R3_DROP_CACHES=1`. Every cold-open report has cache `cold: page cache dropped before the open` and cgroup `memory_max 4294967296`, `cpu_max 200000 100000`. |
| D1 source | a fresh `manual-*.engine` backup (2026-11-23 22:23 UTC, 487 MB) taken through the store daemon (`rusty-remind-me admin backup`), copied read-only. D1 loaded 25,232 memories, 2,575 entities, 3 relations, 4,575 references and 31,940 stored 1024-dim vectors. |
| Query vectors | the 50 #1563 queries embedded once before any benchmark (D59), with `qwen3-embedding:0.6b`, role `query`, dim 1024, bound to that backup. Neither the engine nor SQLite refused them; for both, all 50 were scored with no relevant ids missing. Attest's G2 did not run. |
| Model slot (D38) | no local-model job ran alongside a benchmark command. The live remind-me embedder (ollama on BaileyAi) was unloaded before the run and before every step (Nano's OK); it reloaded for live traffic between steps 9 times and was unloaded again each time. The script found the slot busy before a step 0 times. |
| G1 | the port conformance suites are covered by CI on this sha (CI run 45307874715, success) and by #414's Attest conformance run. Every bench report also passed the two in-run G1 checks (dialog rows refuse an in-place rewrite; readers saw no torn transaction). |

### Attempts, reruns and stops

- **Attempt 1** (09d4489, before 6567195) was abandoned. Every engine bench
  run was killed by the 4 GB cgroup OOM killer, including its one rerun
  (D1 twice, D2 twice). Both engine D3 attempts ended with exit 143
  (SIGTERM, not an OOM kill), and the run was stopped during the engine
  crash run so the D1 loader could be fixed. Those reports are kept on
  BaileyAi, not scored.
- **Attempt 2** (this run) was restarted once in its first minutes (two
  start lines in the run log), before any step had finished.
- **Reruns:** one, by the one-rerun rule. Attest bench D2 exited 1 twice with
  `cold child gave no first answer within 300s`: the G7 cold-open child on
  the 75,000-memory store did not answer its first query inside the
  harness's 300 s limit. No D2 numbers exist for Attest.
- **Stopped early:** the run was stopped during the Attest crash run
  (started 2026-11-24 11:22 UTC, about 3.5 h in, no report). Attest's G6
  run started but did not complete, so there is no valid G6 result, and
  both its G2 runs did not run. Its observed G3 and G7 misses already keep
  Attest from qualifying; the missing runs could not have qualified it,
  and they do not settle the overall result.
- Every other command (all 12 engine and SQLite commands, Attest bench D1
  and D3) exited 0 first time with a valid report: no `valid:false`, no
  `scoring_input:false`, no non-empty `insufficient`.

## Gates

A miss on any gate drops the candidate (§ Thresholds).

| Gate | Threshold | Engine | SQLite | Attest |
|---|---|---|---|---|
| G1 Conformance | 100% | pass (CI; in-run checks 3/3) | pass (CI; in-run checks 3/3) | pass by #414; in-run checks clean on D1 and D3 (D2 did not finish) |
| G2 Relevance | recall@10 and MRR within 2 pt of the engine on both sets | pass (it is the baseline) | **pass**, engine and SQLite only: golden and #1563 identical to the engine in every cell (0.0 pt drop) | did not run (stopped) |
| G3 Steady memory | idle with D3 at most 1.0 GB | **pass** 526 MB | **pass** 42 MB | **miss** 2,512 MB |
| G4 Peak memory | at most 1.5 GB in workloads 1-7 | not established. Observed whole-run `VmHWM` (diagnostic): 2,735 MB (D1 2,071, D2 2,735, D3 2,046, crash 907, crash child 740, crash reopen 1,131) | not established. Observed whole-run `VmHWM` (diagnostic): 2,392 MB (D1 1,715, D2 2,392, D3 1,774, crash 904, crash child 80, crash reopen 904) | not established. Observed whole-run `VmHWM` (diagnostic): 4,082 MB (D1 4,065, D3 4,082) |
| G5 Raw text off RAM | idle D3 / idle D1 at most 1.25x | **pass** 1.06x | **pass** 1.00x | **pass** 0.93x |
| G6 Crash safety | 200/200 kills clean | **pass** 200/200, 0 failures | **pass** 200/200, 0 failures | started, did not complete: no valid G6 result (stopped) |
| G7 Cold open | D1 at most 3 s, D3 at most 5 s | **pass** 2.59 s / 2.96 s | **pass** 1.83 s / 1.74 s | **miss** 174.8 s / 298.4 s; D2 gave no answer in 300 s |
| G8 Backup | D3 restore byte-identical | match within the manifest's scope (284,940 items) | match within the manifest's scope (291,054 items) | match within the manifest's scope (285,882 items) |

**G4 measurement.** `peak_rss_mb` is each process's observed high-water
mark (`VmHWM`) over its whole life. For a bench process that includes
loading D1 through the ports and the harness's own working set, not only
workloads 1 to 7. These figures are a diagnostic. They do not establish the
signed G4 verdict ("at any point in workloads 1 to 7"), and they are not
used here to say that any candidate passes or misses G4. The crash runs'
reopen peaks (engine 1,131 MB, SQLite 904 MB) are under 1.5 GB; the larger
figures come from the bench processes.

**G8 scope.** The restore check compares two manifests (see
`crates/remind_me_core/examples/store_bench/package.rs`). A manifest hashes
the rows it defines: memories (without the volatile access and vitality
counters), wiki pages, references, entities, memory-entity links, entity
relations and chunk vectors. It also hashes every body those rows link to,
re-read from its blob. It leaves out the volatile memory counters, the
feedback, promotions, sessions and outbox tables, and revision history. The
item counts above are matches within that scope, not full coverage of the
signed G8 criterion.

**Engine G6 before the fix (D62).** Before the scoring run, the engine
missed G6: a kill inside an update could leave a new `memory_revisions`
row beside an unchanged memory row (D61). Under D62 the engine was fixed
first (its side stores joined the page's undo/redo) and the fixed engine is
the one scored here. It passed 200/200 kills.

## Scored targets

Reported for the record. § Choosing step 2 counts these only among
candidates that pass every gate, which this run does not establish for any
candidate. A row needing two datasets
counts as met only when both meet it.

| Target | Engine | SQLite | Attest |
|---|---|---|---|
| Keyword p95 at most 50 ms (D1 / D2) | not met: 133.8 / 479.1 ms | not met: 121.8 / 308.7 ms | not met: 155.2 ms / no D2 |
| Semantic p95 at most 100 ms (D1 / D2) | not met: 89.5 / 286.1 ms | not met: 69.9 / 240.7 ms | not met: 94.9 ms / no D2 |
| Hybrid p95 at most 150 ms (D1 / D2) | not met: 294.9 / 889.1 ms | not met: 205.1 / 597.1 ms | not met: 325.6 ms / no D2 |
| Hybrid p95 during ingest at most 2x idle (D3) | met: 1.86x | not met: 3.69x | not met: 2.58x |
| Full vitality pass at most 10 s (D2) | met: 0.29 s | met: 0.30 s | not met: no D2 |
| Entity one-hop / two-hop p95 at most 20 / 100 ms (D1) | met: 0.00 / 0.00 ms | met: 0.01 / 0.00 ms | met: 0.01 / 0.00 ms |
| Single add p95 at most 50 ms (D1) | met: 12.3 ms | met: 4.3 ms | met: 18.3 ms |
| Bulk ingest of D3 at most 30 min | met: 10.6 min | met: 7.3 min | met: 12.8 min |
| Disk at most 2x raw text (D3; `disk_mb_total` over the blob store's raw bodies, D53) | met: 1,252 / 1,012 MB = 1.24x | met: 1,341 / 1,012 MB = 1.33x | met: 1,991 / 1,012 MB = 1.97x |
| Search p95, 4 readers + 1 writer, at most 2x idle (D1) | not met: 8,268 / 1,659 ms = 4.98x | not met: 3,404 / 1,189 ms = 2.86x | not met: 11,148 / 1,638 ms = 6.80x |
| **Targets met** | **6 of 10** | **5 of 10** | **4 of 10** |
| Backup / restore (D3, reported) | backup 1.9 s, restore open 3.6 s | backup 2.5 s, restore open 0.4 s | backup 2.1 s, restore open 321 s |

Entity walks are near zero for every candidate because D1 holds only 3
entity relations, so the two-hop walk has almost nothing to traverse. The
timing says little about a real graph.

Other aggregates: D1 load through the ports took 631 s (engine), 971 s
(Attest); D2 (75,000 memories) took 635 s (engine), 181 s (SQLite). Crash
reopen after the last kill: engine 1.66 s, SQLite 0.09 s.

## Relevance (G2) detail

Both relevance runs scored identically on the engine and SQLite in every
cell. On the golden set (82 queries), `hashed/balanced` recall@10 0.921 and
MRR 0.812; `keyword/balanced` 0.905 and 0.771. On the 50 real #1563 queries
over D1, `vector/balanced` recall@10 0.940 and MRR 0.827; `keyword/balanced`
0.944 and 0.783. Attest's relevance runs did not run.

## Choosing (§ Choosing, applied exactly)

1. Drop every candidate that misses a gate: Attest (G3, G7, as observed;
   no valid G6 result, as its crash run did not complete; its G2 did not
   run). For the engine and SQLite this step cannot
   be completed, because the run does not establish their G4 verdict.
2. Most scored targets met: not reached.
3. Ties to least storage code we own: not reached.

**Recommendation: the rule names no store on this evidence.** For context
only, not as a rule result: SQLite beats the engine on G3 (42 vs 526 MB
idle with D3), G7 and crash reopen, and its observed whole-run `VmHWM` is
lower. The engine meets one more scored target, the ingest-time hybrid
ratio. Attest is far outside G3 and G7, consistent with § Attest readiness
(in-memory indexes rebuilt from the log at open). Whether to establish G4
by a rerun or decide on these numbers is Nano's call.

## Where the data is

On BaileyAi: `/home/baileyrd/r3/reports` (one JSON line per command, its
stderr log and the run log `summary.txt`), `/home/baileyrd/r3/runs` (run
directories) and `/home/baileyrd/r3/vectors.jsonl`. Attempt 1 is kept
beside them as `reports-attempt-1` and `runs-attempt-1`.

## Decision

Nano picked SQLite ("SQLite it is", 2026-11-24), recorded as D75 in
`docs/handoff/DECISIONS.md`. It is his choice, not a gate result: the
§ Choosing rule names no store on this evidence, and G4 was not
re-measured.
The pick does not approve migrating the live store; that is R4, a separate
decision and card.
