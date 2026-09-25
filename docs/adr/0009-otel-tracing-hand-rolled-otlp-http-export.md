# ADR-0009: Optional OpenTelemetry tracing — a hand-rolled OTLP/HTTP exporter, not the OTEL SDK

Status: Accepted
Date: 2026-07-30

## Context

`remind_me_mcp/telemetry.py` (156 lines) is present in the reference and
missing here — found unfiled during a parity sweep (`#77`). It wraps the
real `opentelemetry` Python SDK: a `TracerProvider`, an
`OTLPSpanExporter(endpoint=...)`, and a `BatchSpanProcessor` batching spans
before flushing them to a collector (Jaeger, Tempo, Honeycomb, ...) over
OTLP/HTTP. Off by default (`REMIND_ME_OTEL_ENABLED`), a genuine no-op when
disabled or the optional `opentelemetry` extra isn't installed, instrumented
at exactly four boundaries: every MCP tool call, each sync cycle, each
folder-watcher scan pass, each webhook ingest request.

Porting this literally would mean pulling in the real `opentelemetry`/
`opentelemetry-otlp` Rust crate family. That family's HTTP exporter is built
on an async HTTP client, which means adopting an async runtime (`tokio`) —
this crate has a `tokio` dependency declared in `Cargo.toml` but has never
actually used it anywhere in the codebase; every network client here
(`sync/http.rs`'s hub client, `embedder.rs`'s Ollama client, the webhook and
HTTP API servers) is hand-rolled, synchronous, over `std::net`. Adopting a
real async runtime for telemetry alone — a feature that is off by default —
would be the first genuine async usage in the entire crate, an
architectural shift far bigger than what this feature needs.

## Decision

**A hand-rolled OTLP/HTTP JSON exporter**, matching every other external
protocol in this crate (`sync/http.rs`, `embedder.rs`): a plain
`std::net::TcpStream` POST, no async runtime, no OTEL SDK dependency. The
JSON payload shape (`resourceSpans` → `scopeSpans` → `spans`, hex-encoded
`traceId`/`spanId`, `startTimeUnixNano`/`endTimeUnixNano` as decimal-string
nanoseconds, integer `kind`/`status.code` enums) was confirmed directly
against the OpenTelemetry specification, not guessed from the reference's
SDK usage alone — getting the wire format right is what actually delivers
interop value with a real collector; batching is an optimization on top of
that, not a correctness requirement.

**A dedicated background exporter thread with a bounded channel**, not a
literal batch processor. `telemetry::maybe_span` sends a finished span over
an `std::sync::mpsc::sync_channel` to one exporter thread (spawned once,
lazily, the same `std::thread::Builder::new().spawn(...)` shape
`SyncWorker` already uses) that POSTs one span per HTTP request. This keeps
the property that actually matters — a slow or unreachable collector never
blocks the tool call, sync cycle, watcher pass, or webhook request the span
is timing — without needing a batching queue's own flush-interval/max-batch-
size machinery. A full channel silently drops the span rather than
blocking the caller, matching the reference's "telemetry must never be able
to break the server it's observing" framing exactly.

**Trace/span IDs from the existing `uuid` dependency**, not a new `rand`
dependency: `Uuid::new_v4()` already produces 16 cryptographically-irrelevant
random bytes for a trace ID, and 8 bytes of a second UUID for a span ID —
no new dependency for a single well-scoped need, the same reasoning ADR-0001
and every subsequent ADR in this crate already applies elsewhere.

**A queryable `last_error()` instead of a logging framework.** This crate
has no logging-crate dependency anywhere (`SyncWorkerStatus::last_error` is
the established pattern for "how does a background worker report why it
stopped" — a status field a caller can read, not a log line nothing may be
watching). The exporter thread latches `TRACING_DISABLED` and records the
failure reason in a `Mutex<Option<String>>` on its first failed export,
mirroring the reference's own "any failure disables tracing for the rest of
the run" behavior exactly, just reported the way this crate already reports
every other background worker's failures.

## Alternatives considered

**The real `opentelemetry`/`opentelemetry-otlp` Rust crates.** Rejected:
their HTTP exporter needs an async runtime this crate has never actually
adopted despite `tokio` sitting unused in `Cargo.toml`. Introducing real
async usage for an off-by-default observability feature — while every
request-serving code path in this crate stays synchronous — would be a
much bigger architectural change than the issue asked for.

**A synchronous, blocking POST directly inside `maybe_span`'s `Drop`.**
Rejected: this would add real network latency to every traced tool call,
sync cycle, watcher pass, and webhook request whenever tracing is enabled —
even against a local collector, a stalled or unreachable one would stall
the operation being traced. The reference's own `BatchSpanProcessor` exists
specifically so tracing never blocks the thing it's observing; a
background exporter thread preserves that property without needing the
batching machinery itself.

**Batching multiple spans per HTTP POST.** Deferred, not rejected outright:
the reference's own batching is a throughput/collector-load optimization,
not something either the wire format or this crate's four instrumentation
points require. One-span-per-POST is simpler, still spec-conformant, and
correct; batching can be added later without changing the wire schema or
the public `maybe_span`/`Span` API if request volume ever makes it worth
the added complexity.

## Consequences

- Enabling tracing (`REMIND_ME_OTEL_ENABLED=1`) requires no new mandatory
  dependency and adds no async runtime to the crate; disabled tracing (the
  default) is a single atomic load and an early return — genuinely zero-cost.
- A real OTLP/HTTP collector (Jaeger, Tempo, Honeycomb, or anything else
  speaking the spec) receives spans from this crate exactly as it would
  from the Python reference, verified against the spec directly rather than
  assumed compatible.
- Higher per-request overhead than the reference's batched export under
  heavy load (one HTTP round-trip per span, from a background thread,
  rather than amortized across a batch) — acceptable for an off-by-default,
  local-collector-oriented feature; revisit if a real deployment reports
  this as a bottleneck.
- Wiring the fourth boundary (sync cycle) is left to whichever branch
  merges the sync worker (`#57`) first, since no sync-cycle concept exists
  on `main` yet to instrument — the other three (MCP tool call, watcher
  scan, webhook ingest) are wired in this change.
