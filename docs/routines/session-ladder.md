# Routine: refine settled sessions (ADR-0029)

A scheduled Claude routine runs this once a day. It needs the remind-me connector with the
`full` or `standard` tool profile, or any profile with the `pipeline` group (ADR-0030). It touches nothing outside remind-me.

## Prompt

You are refining captured Claude Code sessions in remind-me. Use only the remind-me tools.

1. Call `remind_me_queue` with `kind: "decompose", batch_size: 20, idle_minutes: 120`. Each entry is one
   half of a capture: the `dialog` half holds the session, so work from that half and skip the
   summary half of the same `capture_id`.
2. For each dialog, read the whole capture with `remind_me_get` and its `capture_id`. For a long one, use
   `remind_me_skeleton_read` or read it in parts. Then extract only these facts, and only where
   the dialog states them, and set each fact's `memory_type` to the kind shown:
   - `decision`: what was chosen, with `metadata.rationale` saying why.
   - `reference`: a pull request, merge or issue and what happened to it, with the number or
     URL in the content.
   - `learning`: a gotcha, meaning what broke or surprised, why, and the fix.
   - `preference`: how a person wants work done, naming who.
   Each fact is one self-contained sentence. Give it a subject, predicate and object when it
   has a clear one, so a later fact can supersede it. List the projects, tools and people it
   names in `entities`, because summaries group facts by entity. Skip chit-chat, status updates and
   anything the dialog only guesses at. If a session has nothing worth keeping, record one
   fact saying so (`learning`, "Session <id>: nothing durable"), so it leaves the backlog.
3. Call `remind_me_decompose` with that `capture_id` and the facts. A session captured again
   after an earlier pass comes back here. Only add what is new, because restating an old fact
   adds noise.
4. Call `remind_me_queue` with `kind: "promotion", rung: "fact_to_scenario"`. For each candidate,
   write a short topic summary that the facts support, and call `remind_me_promote` with
   `rung: "fact_to_scenario"` and all of its `source_ids`. Don't promote to persona.
5. Finish with one line: how many sessions were decomposed, how many facts were written and how
   many scenarios were promoted.
