# ADR-0018: Optional features stay inside `remind_me_core`, for now

Status: Accepted
Date: 2026-08-16

## Context

CI wall clock reached 30m20s, and the work to cut it (#331, #332, #333)
raised a question the fix did not answer: **is the crate layout itself part
of why CI is slow?**

`remind_me_core` is 35,415 lines of `src`, against 5,592 for
`remind_me_mcp`, 4,039 for `remind_me_hub`, 3,518 for `remind_me_remote`,
2,494 for `remind_me_api` and 1,319 for `remind_me_cli`. It is the
workspace, more or less, and everything else depends on it.

Seven of the crate's eight optional features gate code that lives *inside*
it. Cargo's compilation unit is the crate, so enabling one feature
recompiles all 35,415 lines. CI's `features` matrix runs eight legs, each
enabling exactly one feature — so the crate is compiled from scratch eight
times per run to exercise code that is a small fraction of it.

### How small, measured rather than estimated

| Module | Lines | Feature |
| --- | --- | --- |
| `cloud_backup.rs` | 403 | `cloud-backup` |
| `reranker.rs` | 399 | `rerank` |
| `audio_import.rs` | 397 | `audio` |
| `ann_index.rs` | 241 | `ann` |
| `image_import.rs` | 219 | `ocr` |
| `pdf_import.rs` | 112 | `pdf` |
| `embedder.rs`'s `onnx_backend` | 148 | `local-embed` |
| **Total** | **1,919** | **5.4% of `src`** |

An earlier pass at this put the figure at 7.5%. That was wrong, and wrong in
the flattering direction: it counted all 941 lines of `embedder.rs` as
optional when only the 148-line `onnx_backend` module is gated — the rest is
the default embedder resolution every build compiles. The corrected number
is 5.4%, and it makes the imbalance sharper, not softer.

`stack-dumps`, the eighth feature, is deliberately absent from that table.
It is not a module; it is `#[cfg]` blocks threaded through `watchdog.rs`'s
693 lines, with feature-on and feature-off arms sitting next to each other.
It is the one optional feature that is *not* leaf-shaped, and it is the
reason "extract the optional features" is not a single uniform change.

### The modules are already shaped like leaves

This is the part that makes extraction look cheap. Of the six standalone
modules, **five have no intra-crate imports at all** — no `use crate::`, no
`use super::`. The sixth, `reranker.rs`, imports exactly one type
(`crate::models::MemorySearchResult`). Inbound, each is reached from one
call site, except `reranker` at two:

| Module | Called from |
| --- | --- |
| `ann_index` | `vectors.rs` |
| `audio_import` | `importer.rs` |
| `cloud_backup` | `backup.rs` |
| `image_import` | `importer.rs` |
| `pdf_import` | `importer.rs` |
| `reranker` | `queries.rs`, `retrieval.rs` |

They are adapters over third-party libraries — a PDF parser, an AWS SDK, a
C++ ANN index, two neural-network runtimes, a whisper build — with narrow
seams to the domain. Nothing about their content requires them to live in
the same compilation unit as the vitality model and the sync protocol.

## Decision

**Leave them in `remind_me_core` for now.** Record the analysis, the
measurements, and the conditions under which the answer should change, so
the next person asking does not have to re-derive it.

This is a decision to *not* restructure, taken deliberately rather than by
omission — which is why it is filed as an ADR rather than left as a note.

The reasoning:

- **The CI problem was not the crate layout.** The 30m20s was
  cache-action overhead (66% of one job's wall clock spent moving a cache
  to avoid 5m44s of compiling), incremental-compilation artifacts nothing
  read, and full debug info across the workspace's 130-plus
  integration-test binaries. Those
  are fixed, and the longest pole is now ~10m30s. Extracting the features
  would have been an expensive answer to a question that turned out to
  have a cheap one — and would have made the real causes harder to see,
  not easier, by changing the shape of the thing being measured.
- **The eight-way rebuild is now parallel, not serial.** Each feature leg
  is its own runner. Recompiling the crate eight times costs eight
  runners, not eight times the wall clock. The waste is real but it is
  spend, not latency, and it is not what anyone was complaining about.
- **Nothing is currently blocked by it.** No contributor is waiting on a
  build, no feature is hard to add because of where it lives, and the
  optional modules are not accumulating coupling — they have none to
  accumulate.
- **`stack-dumps` would remain regardless.** Extracting the six clean
  modules would leave the one genuinely interleaved feature exactly where
  it is, so the crate would still be feature-gated and still rebuild for
  that leg. The change buys less than the table suggests.

## Consequences

- **`remind_me_core` stays large**, and stays the workspace's single point
  of recompilation. This is accepted, not unnoticed.
- **The eight feature legs keep rebuilding the whole crate.** At current
  sizes that is a few minutes per leg on a dedicated runner. It scales
  with the crate, so it will get worse as the crate grows — see the
  triggers below.
- **The measurements above have a shelf life.** They are true as of
  `v0.2.0`. Anyone revisiting this should re-run them rather than cite
  them; the commands are trivial (`wc -l` over `src`, `grep` for
  `cfg(feature`, `grep -E "^use (crate|super)"` per module).

## When to revisit

Concrete triggers, so this is a decision with an expiry rather than a
permanent excuse:

1. **A feature leg becomes the CI critical path.** Right now `windows`
   (~10m30s) dominates and the feature legs finish inside six minutes. If
   a feature leg overtakes it, the eight-way rebuild is costing latency
   rather than spend, and the calculus changes.
2. **An optional module grows intra-crate coupling.** The five-of-six
   zero-import property is what makes extraction nearly mechanical today.
   The first `use crate::` added to one of them is the moment extraction
   gets harder, and the moment to consider doing it first.
3. **A ninth optional feature.** Each one added inside the crate makes the
   rebuild multiplier worse and the eventual extraction larger.
4. **`remind_me_core` passes roughly 50k lines.** The per-leg rebuild cost
   is linear in crate size and it is already the largest crate by a factor
   of six.

## Alternatives considered

- **Extract the six modules as adapter leaf crates**
  (`remind_me_pdf`, `remind_me_ocr`, …), each depending on `remind_me_core`
  or on nothing, with `remind_me_core` re-exporting behind the same feature
  names. This is the option the analysis was actually about, and it is the
  right one *eventually* — the modules are already shaped for it. Rejected
  now for the reasons above: it solves a problem the CI work already
  solved, and it is six new crates to keep honest for a benefit currently
  measured in runner-minutes rather than wall clock.
- **Split `remind_me_core` by domain** (memories / search / wiki / sync /
  entities). Rejected, and not just deferred. These are not independent:
  search reads vitality, sync moves memories *and* entities *and* wiki
  pages, and the schema is one database. Domain splits here would produce
  crates with thick mutual interfaces — the shape that makes a workspace
  slower to compile and harder to change at once, which is the opposite of
  both goals. The optional adapters are extractable precisely *because*
  they are not domain code.
- **Split the repository.** Rejected firmly. This is a modular monolith
  with one schema version, one release, and one set of tests that
  cross-check crates against each other (`remind_me_core`'s dev-dependency
  on `remind_me_hub` for `MockHub`, the schema-drift check against the
  reference). Multiple repositories would turn every one of those into a
  version-coordination problem in exchange for nothing.
- **Move the optional features behind a runtime plugin boundary**
  (dynamic loading, subprocesses). Rejected: it trades compile-time cost
  for runtime failure modes, ABI concerns and a distribution story, in a
  project whose tenet 1 is predictable performance and whose release is
  five self-contained archives.
