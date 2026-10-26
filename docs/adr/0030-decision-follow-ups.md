# ADR-0030: Decisions get a follow-up review, stored in their metadata

Status: Accepted (2026-10-26); implemented
Date: 2026-10-26

## Context

A decision carried an `outcome` only when someone resolved it, and that
outcome (`done`, `abandoned`, `reverted`, `superseded`) says whether it was
carried out, not whether it was right. Nothing asked later whether a choice
held up, so the vault recorded what was decided but built no track record.

## Decision

1. **A review is scheduled when a memory becomes a decision**: on add, on an
   update or reclassify that changes its type to `decision`, and on decompose.
   It falls due `REMIND_ME_DECISION_REVIEW_DAYS` later (default 21; `0` turns
   scheduling off). `metadata.stakes: "low"` skips it. Decisions written
   before this change are not backfilled.
2. **The review lives in `metadata`, not in columns.** Engine records are
   positional (ADR-0026 §3), so a column is a schema bump and an open-time
   migration. `metadata.review = {due, outcome?, note?, reviewed_at?}`
   syncs and keeps history like any other metadata. `kinds.rs` validates it
   against closed enums: `stakes` ∈ {low, normal}, `review.outcome` ∈
   {held, revised, reverted}.
3. **Held up and carried out stay two axes.** `outcome` is unchanged; the
   review is separate. The one link: a `reverted` review also resolves the
   decision as `reverted`, with its demotion, since it was undone either way.
4. **The state is derived, never stored**: `review::ReviewState` is one of
   unscheduled, low_stakes, pending, due, held, revised, reverted, closed
   (abandoned or superseded before a review).
5. **Not a reminder.** `remind_at` is the user's one reminder slot; writing
   reviews into it would overwrite their reminders and fire a notification
   per decision. `review::due_among` is the one definition of "due", read by
   the digest ("Decisions to review") and the session brief (a section after
   reminders), as `reminders::list_reminders` is for reminders.
6. **No new tool.** `remind_me_resolve` takes `review` in place of `outcome`
   (exactly one); the CLI takes `resolve <id> --review <outcome>`.
   `remind_me_stats` gains a `decisions` report: counts by state, overall and
   per project and month written.

## Consequences

- Every decision written from now on shows up for review about three weeks
  later, until someone records what happened.
- Replacing a decision's whole `metadata` in an update drops its `review`,
  as it drops every other key the caller leaves out.
- "Due" is computed by scanning live decisions; there is no index on the
  review date. Decisions are a small share of a vault, so this is cheap.
