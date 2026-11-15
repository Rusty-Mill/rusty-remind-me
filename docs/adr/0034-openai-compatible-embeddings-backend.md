# ADR-0034: A third embedding backend for OpenAI-compatible servers

Status: Accepted; implemented by PR #346 (2026-10-29), recorded afterwards (#209)
Date: 2026-11-15

## Context

ADR-0002 chose Ollama's `/api/embed` as the first embedding backend because it
added no compile-time dependency, and said any further backend is "its own
decision and its own ADR". The `onnx` backend followed. Many local servers
(llama.cpp's `llama-server --embeddings`, llama-swap in front of it, vLLM,
LM Studio, TEI) do not speak Ollama's API but all serve the OpenAI shape,
`POST /v1/embeddings`, so they were unreachable (#209). The node's own
llama-swap is one of them.

## Decision

1. **`REMIND_ME_EMBEDDING_BACKEND=openai`** selects `OpenAiEmbedder`
   (`crates/remind_me_core/src/embedder.rs`). `REMIND_ME_OPENAI_EMBED_URL`
   names the server (default `http://localhost:8080`; a trailing `/v1` is
   accepted) and `REMIND_ME_OPENAI_EMBED_MODEL` the model it routes on. The
   model is required: a blank one makes the embedder unusable rather than
   resolved, so stored vectors are kept (#351).
2. **No new dependency.** It uses the same hand-rolled HTTP client as
   `ollama` (`post_json`). Only the request path and the response shape
   differ, so ADR-0002's argument holds unchanged: the backend is small and
   auditable, and the build gains nothing.
3. **Local network only: plain `http://`, no API key.** Like `ollama`, it is
   meant for a daemon the operator runs. This keeps every secret out of the
   embedder, its errors, its logs and `embedding_meta` by construction rather
   than by redaction. Hosted providers, which need TLS and a key, are out of
   scope; reaching one is a new decision.
4. **`EmbedRole` is not ignored.** The OpenAI request has no role field, so
   the backend applies the same model-family instruction prefixes as `ollama`
   and `onnx` (`ROLE_PREFIXES`, `with_prefix`): a query to a Qwen3-Embedding
   model carries its instruction and a passage goes in bare. A model not in
   the table gets no prefix on either side.
5. **Same checks as the other backends.** Each vector must have
   `REMIND_ME_EMBEDDING_DIM` elements, with the same error naming the model
   and the setting (`checked_vector`). Entries are placed by their `index`,
   and a missing, repeated or out-of-range index is an error. The identity is
   `("openai", model, dim)`, so a model change clears vectors as ADR-0002
   intends.

## Consequences

- Any OpenAI-compatible server on the local network can embed, with no code
  change per server.
- `crates/remind_me_core/tests/openai_embedder_test.rs` covers the round
  trip against a loopback stub, the dimension check, index handling, chunked
  responses and the Qwen3 role prefix; `open_reconcile_test` covers the
  missing-model case.
- Pointing it at a hosted provider fails on the `http://`-only check before
  any request is sent.
