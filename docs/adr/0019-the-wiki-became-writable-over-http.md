# ADR-0019: The wiki became writable over HTTP

Status: Accepted
Date: 2026-08-16

## Context

FT-08 added a REST surface for the wiki, and it was read-only on purpose.
`routes.rs` said so in as many words:

> The wiki tools are LLM-curated by design (see SCHEMA.md's "you are the
> disciplined maintainer" framing): Claude can write and browse it, but a
> human owner has had no way to *see* it outside the MCP tools. This mirrors
> only the read paths — there is **deliberately no POST/PUT/DELETE here**.

#328 added `POST /api/wiki`, `DELETE /api/wiki/{slug}` and
`POST /api/wiki/compile`, which reverses that. The reversal was made, the
reasoning was written into the module docs and `RELEASE_NOTES.md`, and no
ADR was filed — so the *decision* lived only inside the change that made it.
`ATLAS-GOV-ADR-0001` asks for an ADR at decision time; this one is
retroactive, and says so.

It is worth filing late rather than not at all, because the note being
reversed was not incidental. It was a design posture stated in the
imperative, and a future reader finding write routes next to a comment that
once forbade them deserves better than a diff to reconstruct why.

## Decision

**Hand the vault's owner the mechanical operations over HTTP. Leave the
synthesis where it was.**

The distinction is the whole argument, and it is what makes this a
refinement of the original posture rather than an abandonment of it:

- **"LLM-curated" describes who does the *synthesis*.** That has not
  changed. `Wiki::compile` still returns a *brief* — the maintainer schema,
  the page index, and the memories written since the watermark — for a model
  to act on. Nothing added in #328 writes a page's prose on its own.
- **What the read-only posture also did**, incidentally rather than by
  design, was leave the owner of the vault unable to fix a typo, delete a
  page a compile pass got wrong, or write one by hand without opening an MCP
  client. That is an editorial veto a person should have over their own
  notes, and withholding it was never the point of "LLM-curated".

So: writes, deletes, and advancing the compile watermark are exposed. Judgment
about *what a page should say* is not, and no route added here forms an
opinion about content.

## Consequences

- **The daemon's write surface is wider than memories**, and now includes
  files on disk rather than only rows in SQLite. `ARCHITECTURE.md` names
  this in the crate-roles section rather than leaving it implicit.
- **Every write is API-key-gated.** Mutating methods are refused with 401
  while `REMIND_ME_API_KEY` is unset — the general posture in
  `remind_me_api`'s module doc, and load-bearing here specifically: a store
  that could previously only be *read* over HTTP can now be written to, and
  that must not default open.
- **Containment is structural, not validated.** Every path is
  `wiki_root/{slug}.md`, and the slug comes from `wiki_import::slugify`,
  which keeps ASCII alphanumerics and turns everything else into `-`. A
  title therefore cannot contain a separator, a `..`, a leading dot or an
  extension, whatever a caller sends — not because a check rejects those,
  but because the transformation cannot produce them. That is the stronger
  property, and it is why there is no path-traversal check to forget to
  apply on a future route.
- **The three generated slugs are refused.** `index`, `log` and `schema`
  (`wiki::RESERVED_SLUGS`) are maintained automatically; `index.md` is
  regenerated on every write, so a hand-written one would sit permanently at
  odds with the pages it claims to list.
- **Length bounds mirror the MCP tool rather than being invented.** Title
  200, content 100,000, log note 500 — `remind_me_wiki_write`'s own
  JSON-schema bounds. Core enforces none of them; `Wiki::write_page` writes
  whatever it is handed. A bound that existed only on the tool would mean
  the same page could be written over HTTP and then rejected as too long
  over MCP.
- **Compile stays two-phase, with the safe phase as the default.** The brief
  (`mark_integrated: false`) is idempotent and advances nothing; a separate
  call moves the watermark after pages are written. Exposing this over HTTP
  does not collapse the two phases into one convenient button.
- **23 integration tests** in `wiki_write_test.rs` cover the write, delete
  and compile paths, including the auth refusals, the reserved slugs, the
  bounds, and that a title cannot escape the wiki root however it is spelled.

## Alternatives considered

- **Leave it read-only and keep edits in the MCP client.** The status quo,
  and defensible: it keeps exactly one writer. Rejected because it makes the
  human owner's access to their own notes contingent on having an MCP client
  open, for operations — fix a typo, delete a bad page — that need no model
  at all.
- **Expose writes but not deletes.** Rejected as the worse half of a pair. A
  compile pass that produces a wrong page is precisely the case the owner
  needs to correct, and "you may add but never remove" turns the vault into
  an append-only pile, which is the opposite of the "revise in place"
  instruction the maintainer schema itself gives.
- **Let the HTTP surface synthesise** — a route that takes raw memories and
  writes pages autonomously. Rejected firmly, and it is the line this ADR
  exists to draw. That would abandon the original posture rather than refine
  it. `compile` returning a brief for a caller to act on is the boundary.
- **A separate write daemon or a distinct port.** Rejected: the auth posture
  already distinguishes reads from writes by method, and a second listener
  would double the deployment surface to express something one 401 already
  expresses.

## A loose end this turned up

The `routes.rs` comment quoted above cites "SCHEMA.md's 'you are the
disciplined maintainer' framing". **There is no `SCHEMA.md` in this
repository**, and that phrase appears nowhere in it — the citation appears
to point at the reference project rather than anything a reader here can
open. The in-repo equivalent is `DEFAULT_SCHEMA` in
`crates/remind_me_core/src/wiki_fs.rs`, seeded as the wiki's `schema.md` on
first read, which carries the actual curation rules ("Distil, do not paste",
"Revise in place", "Flag contradictions explicitly").

Left as-is rather than silently rewritten, because changing a comment's
citation is a separate change from recording this decision, and because the
reference may well say what the comment claims. Worth fixing in its own
right.
