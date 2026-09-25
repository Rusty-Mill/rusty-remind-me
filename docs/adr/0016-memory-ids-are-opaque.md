# ADR-0016: Memory ids are opaque, and both formats are valid in a shared store

Status: Accepted
Date: 2026-08-07

## Context

`rusty_remind_me` and `remind_me` write structurally different primary keys
into the same `memories.id` column of the same database.

| | Scheme | Example |
| --- | --- | --- |
| `remind_me` | `hashlib.sha256(f"{content}{ts}").hexdigest()[:12]` (`db.py:3215`) | `b14392f2f0aa` |
| `rusty_remind_me` | `format!("mem_{}", uuid::Uuid::new_v4().simple())` (`db/queries.rs:101`) | `mem_c8fb9e374f134c73b6189eede41bfe42` |

Twelve hex characters against a four-character prefix plus thirty-two. Derived
from content against drawn at random.

This was not found by reading either codebase. It surfaced while checking
whether the port is genuinely drop-in against a live reference-created database
(#218), which it is: the port wrote a `mem_`-prefixed row into a v29 file the
reference had created, the reference re-opened that file and read the row back
with every column intact, and wrote its own row afterward. `id` is opaque
`TEXT` with no length constraint, no format check, and no parsing on either
side, so each tolerates the other's keys.

ARCHITECTURE.md Tenet 3 promises "identical SQLite schema and JSON tool
signatures". The column types are identical. The *values* are not, and nothing
anywhere said whether that was intended.

## The problem worth solving

Not the divergence. The **silence** about it.

"It happens to work" and "it is specified to work" are different states, and a
shared database currently accumulates two id conventions with nothing recording
that this is expected. That gap is what lets a reader draw a reasonable but
unsupported conclusion — that `mem_` is a writer tag they can dispatch on, that
ids are twelve characters, that an id can be recomputed from content — and
build on it.

## Decision

**Ids are opaque. Both formats are valid in a shared store. Neither
implementation may parse, measure, or pattern-match another implementation's
id.** Specifically:

1. **The `mem_` prefix is not a contract.** It is not a provenance marker, and
   nothing may branch on it. A future change may drop it without that being a
   breaking change.
2. **Length is not a contract.** Anything that assumes twelve characters, or
   thirty-six, is wrong today for half the rows in a shared database.
3. **Ids are not derivable.** The reference's is a function of content and a
   timestamp; the port's is not a function of anything. Code that recomputes an
   id to find a row is correct against at most one implementation.
4. **Neither scheme changes.** The port keeps uuid4.

## Why not adopt the reference's scheme

It was the obvious candidate for "make the port match", and it is rejected on
the merits rather than on cost.

The reference's id is `sha256(content + timestamp)[:12]`. Two identical
memories added within the same timestamp resolution collide — the id is a
function of exactly the inputs that a duplicate shares. uuid4 cannot collide
this way. Adopting the reference's scheme would import a real failure mode to
gain a cosmetic consistency in a column neither side parses.

It would also rewrite nothing and fix nothing: every `mem_`-prefixed row
already written would keep its id forever, so the database would still hold two
formats. The change would buy uniformity for *future* rows only, at the cost of
a collision mode, in a column whose whole contract is that nobody looks inside
it.

Truncating a hash to twelve hex characters is also 48 bits, which is a birthday
collision at roughly 16 million rows independent of the content-plus-timestamp
issue. That is not a problem the reference has in practice, and not one worth
adopting deliberately.

## Consequences

- The interop that previously held by accident is now pinned by
  `id_format_test.rs`, which drives both formats through the real read, update
  and delete paths. If a future change starts parsing ids, those tests fail.
- Display surfaces will show mixed id lengths in a shared store. Accepted; the
  alternative is a rewrite of historical ids, which is worse.
- `remind_me`'s own schemes for **entity** ids (`sha256(normalized_name)[:12]`)
  and **relation** ids (`sha256("subject|relation|object")[:12]`) are
  deliberately *not* covered by this ADR. Those are content-addressed on
  purpose — the determinism is the mechanism by which two nodes agree on the
  same entity without coordinating — and the port matches them exactly. This
  ADR is about `memories.id`, where nothing depends on derivability.
