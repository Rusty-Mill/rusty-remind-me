# ADR-0033: The wiki compiles itself with a local model

Status: Accepted (2026-11-05); implemented
Date: 2026-11-05

## Context

`remind_me_wiki_compile` hands a model a brief of pending memories and
waits for it to write the pages and mark the brief integrated. Only a
person carrying that brief to a chat session ever ran it, so pending
sources piled up. The node's own hardware can run a small local model
behind an OpenAI-compatible server (llama.cpp, vLLM, Ollama), which is
enough to write a page from a dozen memories without anyone in the loop
(card #747).

## Decision

1. **One run, on demand.** `remind_me_wiki_compile` with `auto=true` and
   `rusty-remind-me wiki-compile --auto` compile up to `max_batches`
   batches (1 over MCP, 5 on the CLI) and return a report. There is no
   background loop: a timer can call the CLI, and a run that writes pages
   unasked should be one someone chose to schedule. There is no HTTP
   route: a request held open for minutes is the wrong shape for the
   dashboard, which can poll the pending count instead.
2. **Policy in core, the model behind a port.** `wiki_compile` holds the
   grouping, the guards and the run loop, and talks to a `WikiWriter`.
   `wiki_llm::ChatCompletionsWriter` is the only adapter; it POSTs to
   `/v1/chat/completions` through the embedder's existing HTTP client, so
   no dependency is added and `http://` only, as for Ollama.
3. **Sources are grouped by page with the embedder.** A source joins the
   closest existing page at cosine `REMIND_ME_WIKI_MATCH` (default 0.55)
   or above, and a second page within 0.05 of that. Sources that match no
   page are clustered among themselves, and each cluster becomes a new
   page the model names. Without an embedder, or if embedding fails, the
   whole batch is one new-page group and the model decides. A group over
   12,000 characters is written in several calls on the same page.
4. **A batch is all or nothing.** The batch is the same one a brief shows
   (`Wiki::pending_batch`), and it is marked integrated through its
   `through` only after every page is saved. A model error, a refused
   draft or a source no written page cites stops the run with the batch
   still pending, so the next run hands the model the same sources. Pages
   written before the failure stay written; the next run rewrites them.
5. **Guards against losing what a page held.** A draft is refused when it
   is empty, renames the page, or, unless `allow_shrink`, keeps less than
   60% of the page's text or drops one of its `[[links]]`. A new-page
   draft that names an existing page is checked as a rewrite of it. Every
   source id must appear in the saved text of some page the batch wrote,
   checked after the last write, so a later chunk cannot drop a citation
   an earlier one made.
6. **One run at a time.** A run claims its wiki root in an in-process
   set for as long as it runs, and a second run on the same wiki reports
   and returns before calling the model. In-process is enough because
   only one process opens a store (the engine holds a lock for it), so
   the claim is atomic, never expires under a slow model, and is released
   only by its holder, on return or unwind.
7. **Configuration.** Off unless both `REMIND_ME_WIKI_LLM_URL`
   (`http://host:port`) and `REMIND_ME_WIKI_LLM_MODEL` are set.
   `REMIND_ME_WIKI_LLM_TIMEOUT_SECS` (default 600) bounds one page and
   `REMIND_ME_WIKI_LLM_MAX_TOKENS` (default 4096) its length; a reply cut
   off at the limit is a failure that names the setting. The model must
   answer with a JSON object `{title, content}`; the first `{` to the last
   `}` after any `<think>` block is parsed, and the prompt ends in
   `/no_think` for Qwen models.

8. **The database lock is not held while the model writes.** A store
   handle holds the database lock while it lives, so a run takes the
   database and opens a short handle for each step: reading the batch,
   saving a page, marking. Other tools are served in between. Before a
   page is saved, it is re-read under the same handle that saves it; if
   it changed since the model was shown it, the batch stops instead of
   overwriting the edit. `REMIND_ME_WIKI_LLM_TIMEOUT_SECS` is a deadline
   for the whole call, not an idle timeout, so a server that trickles
   bytes cannot hold a run open.

## Consequences

- Pending memories reach the wiki without a person, at the cost of page
  quality bounded by the local model. The guards keep a weak model from
  erasing a page, not from writing a dull one.
- A slow model makes an MCP call take minutes. The default of one
  batch keeps that bounded; the CLI is the place for longer runs.
- The settings are `REMIND_ME_*`, so a client and the store daemon must
  agree on them like every other store setting (ADR-0023 §2).
