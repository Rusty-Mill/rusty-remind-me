# ADR-0027: Projects are entities with evidenced claims

Status: Accepted (2026-10-26, owner approval); not yet implemented
Date: 2026-10-26
Board: Mill Board #57

## Context

A project today is only a name: an entity with aliases and an optional free
`kind`. Nobody, human or model, can ask "is this something we only talked
about, or is there a repository?" and get an answer backed by evidence. The
pieces to answer it already exist but are not joined:

- The entity graph (`entity.rs`): deterministic ids, union-merged aliases,
  relations, synced across nodes.
- Subject-predicate-object facts, where a new object for the same
  `(subject, predicate)` supersedes the old one
  (`supersede_contradicting_facts`).
- Context capture (ADR-0026): every memory records `project`, `git_remote`,
  `cwd`, `session_id`.
- The refinement ladder (`promotion.rs`): read-time candidates, written only
  when an agent hands the result back.

Constraints from neighbouring cards: #54 shrinks the tool surface, so this
must add at most one tool. #65 needs plain JSON any model can use. #55 makes
rusty_tick the one task store, so a Mill Board track is not a durable anchor.
The board's autonomy policy forbids the robot from running database
migrations, and a new synced table would also need a sync-contract change.

## Decision

### 1. A project is an entity with a live stage claim

No new table and no schema bump. An entity *is* a project when it has a live
`project.stage` claim. `entity.kind` stays a hint; it is never overwritten
(existing rule), so it cannot be the source of truth.

### 2. Claims are memories with a closed predicate set

Each piece of project state is one fact memory:

| predicate         | object (validated at the boundary)              | cardinality |
|-------------------|-------------------------------------------------|-------------|
| `project.stage`   | `idea` \| `designed` \| `built`                 | one         |
| `project.status`  | `active` \| `dormant` \| `abandoned`            | one         |
| `project.repo`    | normalised repo URL `https://host/owner/name`   | one         |
| `project.path`    | repo-relative path, e.g. `crates/apps/rusty_mux`| one         |
| `project.tracker` | `tick:list:<list uuid>`, the project's tick list | one         |
| `project.parent`  | parent project's entity name                    | one         |

`subject` is the project's canonical entity name. Aliases stay on the entity.
`project.tracker` follows the root ADR-0012 (one task store): a board project
is a rusty_tick list, and remind-me refers to tick objects with the `tick:`
scheme. ADR-0012 gives tasks `tick:<task uuid>`; a list here is
`tick:list:<list uuid>`, so the reference detector's task rule never takes a
list for a task.

Writing a claim supersedes the previous one through the existing SPO rule.
Claim memories are `memory_type: fact`, tagged `project-record`, and synced
like any memory. Claim memories are never edited after they are written.

**History and reversal are new work, not existing tools.** `remind_me_history`
and `remind_me_revert` (`history.rs`) record and roll back edits *within* one
memory: content, category, tags, metadata and sensitive. They do not track
SPO values or `superseded_by`, and `supersede_contradicting_facts` writes no
revision. So they cannot show or undo a stage change. This ADR defines its
own history and reversal:

- **Claim history** is the chain of claim memories for one
  `(project, predicate)`, superseded ones included, ordered by `created_at`
  then id. `project::history(store, name, predicate?)` returns it with each
  claim's value, evidence, `confirmed_by` and which claim superseded it. No
  new storage is needed: superseded memories are kept, not deleted.
- **Reversal is a compensating claim.** Undoing a claim writes a *new* claim
  that restores an earlier claim's value. Its metadata names the claim it
  reverts (`"reverts": "<claim id>"`), and it cites that earlier claim's
  evidence plus the reverting caller's `because` memory. Nothing is
  un-superseded, edited or deleted, so the history stays append-only, every
  node converges by the newest-live-claim rule (section 3), and the reversal
  is itself evidenced. Reverting the first claim of a project (no earlier
  value) is refused for `stage` and `status`. A project leaves the list by a
  `status = abandoned` claim, never by deletion.

`metadata` carries what makes a claim trustworthy:

```json
{
  "project_claim": {
    "evidence": ["<memory or capture id>", "..."],
    "confirmed_by": "human",
    "proposal": "<proposal key, if it came from one>",
    "reverts": "<claim id, only on a compensating claim>"
  }
}
```

Why memories rather than a `projects` table: sync, supersession, retention of
superseded values and search come for free; there is no migration and no
sync-contract change. The cost is that the typed guarantee lives at the
boundary and in the projection, not in a column type. That is the same
trade ADR-0026 made for per-kind data.

### 3. The typed model is the projection

`remind_me_core::project` reads live claims and builds this. Nothing outside
the module constructs it.

```rust
pub enum Stage { Idea, Designed, Built }
pub enum Status { Active, Dormant, Abandoned }

pub struct Evidence(NonEmpty<MemoryId>);          // at least one id, always

pub struct Claim<T> {
    pub value: T,
    pub evidence: Evidence,
    pub claim_id: MemoryId,                       // the claim memory itself
    pub confirmed_at: DateTime<Utc>,
}

pub struct Project {
    pub entity: EntityRef,                        // id, name, aliases
    pub stage: Claim<Stage>,
    pub status: Claim<Status>,
    pub anchors: Anchors,
}

pub struct Anchors {
    pub repo: Option<Claim<RepoUrl>>,             // newtype, normalised
    pub path: Option<Claim<RepoPath>>,            // relative, no `..`
    pub tracker: Option<Claim<TickListId>>,        // `tick:list:<uuid>`
    pub parent: Option<Claim<EntityId>>,
}
```

- Every stage-by-status combination is legal (an abandoned idea, a dormant
  built project), so a pair of enums is exact.
- A project without evidence cannot be built: `Evidence` has no empty value.
- The write path sets stage and status together when a project is created, so
  a project never has one without the other. If a synced store ever holds a
  malformed or half-written project, the projection reports it under
  `invalid` with the reason. It never drops it silently.
- Concurrent edits on two nodes: the projection takes the newest live claim
  per predicate (ties by id), so every node agrees without relying on which
  supersession ran where.

### 4. Evidence is required on every write

A confirm carries the proposal's evidence. A direct `set` needs `evidence`
ids or a `because` sentence. A `because` sentence is first saved as a fact
memory written by the caller, and that memory becomes the evidence. So "Nano
said so" is recorded as evidence and can be found later.

### 5. Proposals are computed when read; only a person confirms

Like `promotion_candidates`, proposals are computed from the store and are
never stored. Each one has a stable key: `hash(entity_id, predicate, value)`.

Version 1 rules, all deterministic and with no LLM call:

| rule | proposes | evidence |
|------|----------|----------|
| entity with kind `project`, `app`, `crate` or `repo`, or 3+ linked memories, no stage claim, and no repo or path evidence | `stage = idea`, `status = active` | the linked memories |
| memories linked to the entity carry a `git_remote` whose repo name matches the name or an alias | `stage = built`, `repo = <remote>` | those memories |
| memories linked to the entity carry a `cwd` under `<repo>/crates/<group>/<name>` | `path = crates/<group>/<name>` | those memories |
| a linked `decision` memory with tag `design` or `adr`, and stage `idea` | `stage = designed` | that decision |
| `status = active` and no linked memory for 60 days (configurable) | `status = dormant` | the last linked memory |

A proposal never moves a project backwards (built to idea) on its own; only a
direct `set` can. Rules improve as #56 feeds more session facts in, but they
do not depend on it.

**Confirm** writes the claim (or the stage and status pair).
**Reject** writes one `work_log` memory tagged `project-proposal-dismissed`
with the key in its metadata. It syncs, and that key is not proposed again. A
different value gets a new key, so new evidence for a different stage is
still shown.

Nothing promotes automatically, including in the backfill.

### 6. One read surface, one tool

- Core: `project::list(store) -> ProjectList { by_stage, proposals, invalid }`
  and `project::get(store, name)`. Shared by MCP and HTTP, the way
  `entity_profile` is, so the dashboard and every model see the same data.
- MCP: one tool, `remind_me_project`, with `action`:
  `list` (default) | `get` | `history` | `proposals` | `confirm` | `reject` |
  `set` | `revert`.
  `list` returns compact JSON grouped by stage, for example
  `{"built":[{"name","status","repo","path","evidence_count"}]}`; evidence ids
  come from `get` and `history`, so a listing stays cheap to read (#65).
  It belongs in the core tier, but `core` is deliberately held below twenty
  tools, so it ships in `standard` until the tool-surface audit (#54) decides
  what leaves. One tool, one tier: the writes are not split out, because a
  confirm is the point of the read.
- HTTP: `GET /api/projects` for every read (`action` selects `get`, `history`
  or `proposals`) and `POST /api/projects` for every write. A writing action
  over `GET` is refused with 405 rather than performed.
- Dashboard: a Projects view with columns Idea, Designed and Built, a status
  chip, links for each anchor, an evidence count, a "Pending" lane with
  confirm and reject buttons, and anything the projection could not read.
- `remind_me_brief` (#62) reads `project::get` for its header.

### 7. Backfill is proposals, not writes

`remind-me project backfill [--seed-repo PATH]`:

1. Runs the rules over every existing entity and prints the proposals. It
   writes nothing.
2. With `--seed-repo`, it reads the workspace's crate directories, such as
   `crates/apps/*`. For each one that matches an entity, it saves a
   `reference` memory ("rusty_mill has `crates/apps/rusty_mux` at `<sha>`")
   as evidence, which turns into a `built` and `path` proposal.
3. Nano confirms the results in bulk from the dashboard's Pending lane.

The only writes are the reference memories in step 2. They are ordinary
memories and can be deleted like any other. Nothing is migrated.

## Consequences

- Project state is auditable and reversible through `project::history` and
  compensating claims (section 2). Both are new code, about the size of the
  projection, and they need no new storage.
- No schema, sync-contract or migration work, so the board robot can build
  it.
- Claim memories show up in search. They are short and tagged, and #51's
  compact results keep them cheap. If they add noise, search can exclude
  `project-record` by default. That is one filter, not a redesign.
- The typed guarantee is enforced in one module, not by storage. Writing a
  `project.*` triple directly through `remind_me_add` must be refused: the
  generic write paths reject the `project.` predicate prefix. This is a
  required part of the build, not a follow-up.
- Repository-level anchors stay single-valued. A move, such as remind-me
  leaving the monorepo (#61), is a superseding claim, and the history keeps
  the old one.

## Alternatives considered

- **A `projects` engine table with typed columns.** Enforces the types in
  storage, but needs a new record type, an outbox payload, a sync-contract
  version, an open-time upgrade and a migration the robot cannot run. Worth
  it only if claims-as-memories proves too noisy or too slow. There are tens
  of projects, not thousands.
- **Use `entity.kind = "project"` as the marker.** Kind is fill-once and never
  overwritten, so a wrong guess could never be corrected.
- **LLM-proposed stages in `remind_me_decompose`.** Not deterministic, not
  testable, and small local models would disagree with each other. Agents
  can still call `set` with `because`.
- **Mill Board track as an anchor.** #55 retires the board's own store, so the
  anchor is the project's rusty_tick list (`tick:list:<uuid>`) instead.

## Implementation slices (after approval)

1. `project` module: enums, newtypes, `Evidence`, projection, guarded writes,
   claim history, compensating-claim revert, and refusing `project.*`
   predicates on the generic write paths. Unit tests
   for every rule and invalid state.
2. Proposal rules, rejection keys, and the backfill command (dry run plus
   `--seed-repo`).
3. `remind_me_project` tool, HTTP routes, and the dashboard Projects view.
