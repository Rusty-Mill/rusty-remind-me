# Stated facts and inferences (Mill Board #1553)

`written_by` and `capture_method` say who wrote a memory and how. They do not
say whether its content is something a person said or something a model
concluded. `metadata.basis` says that.

| `basis` | Meaning | `metadata.premises` |
|---|---|---|
| `stated` | Someone said it. | none |
| `inferred` | A model concluded it from other memories. | 1 or more memory ids |
| `pattern` | Seen across several sources. | 2 or more memory ids |
| unset | Written before `basis` existed, or by a writer that did not say. | none |

No schema change: both keys live in `metadata`, checked at the boundary by
`kinds::validate_basis` on every add, update, decompose and reclassify.

## Who sets it

- `remind_me_promote` sets `pattern` (two or more sources) or `inferred` (one),
  with the sources as premises. A distillation is always a model's conclusion.
- `remind_me_decompose` passes each fact's `metadata` through; its description
  asks the writer to say `stated` or `inferred`.
- Any `remind_me_add` may set it.

## What it changes

- **Conflicts.** `contradictions::preferred` ranks basis first: stated, then
  unset and pattern, then inferred; then confidence, verification and recency
  as before. The contradiction queue's `recommended_keep` and the session
  brief's hits follow it, so the brief never shows both sides of one claim.
- **Supersession.** A new `inferred` triple does not retire a contradicting
  `stated` one; both stay live and the queue prefers the statement. Every
  other write supersedes as before, so a person correcting themselves (unset
  or stated) still retires the old fact.

## How it maps onto #1442's trust tiers

The two are orthogonal and compose:

- **Tier** (#1442, not built) is about the *channel*: how far to trust the
  writer (`written_by`/`source`: human > model > hook > importer).
- **Basis** is about the *content*: whether it was said or concluded.

A model transcribing what a person said writes `stated` at model tier: the
claim is a statement, and the tier says how far to trust the transcription.
A human writing a guess writes `inferred` at human tier.

When #1442 lands:

1. Search weighting multiplies a tier factor by a basis factor (inferred
   lowest), both small and tunable.
2. Quarantine candidates are `inferred` memories from the lowest tiers
   (hook, importer) whose premises are missing or themselves quarantined.
   Content-based detection stays out of scope, as #1442 says.
3. A `stated` memory from an untrusted channel is still untrusted: basis never
   raises a tier.
