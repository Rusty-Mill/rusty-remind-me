# ADR-0014: Watchdog stack dumps — optional, Linux-only, out-of-process

Status: Accepted
Date: 2026-08-05

## Context

The reference's `watchdog.py` (its issue #128) arms
`faulthandler.dump_traceback_later`: after a timeout it dumps **every thread's
stack**, from a separate OS thread, including a thread wedged in synchronous
CPU-bound code. That last clause is the whole point. The incident that
motivated it was a correlated subquery pegging a core for minutes, and
diagnosing it meant attaching `py-spy` to a live process. An `asyncio` timer
cannot do this job, because a blocked event loop cannot run its own watchdog.

The Rust port shipped the reference-counting, the threshold, the env switch and
the `remind_me_server_status` reporting, but not the dump — it reported the
stuck call's *identity and duration* instead. `gap-analysis.md` listed
"a stack-dumping crate for D1" as a stop-and-ask item, on the same footing as
ADR-0012's `libc` and ADR-0013's `windows-sys`: a new dependency taken for a
diagnostic.

This ADR records the answer, which was **yes, but not on the terms the
gap-analysis assumed.**

## The finding that changed the shape of the decision

"A stack-dumping crate" understates it. There is no pure-Rust way to dump
another thread's stack. The two real options are:

1. **In-process, via a signal handler.** Send a signal to the stuck thread and
   unwind it in the handler — what `pprof-rs` does for profiling. It needs no
   system library.
2. **Out-of-process, via `ptrace`.** Spawn a short-lived child that traces this
   process and walks every thread. `rstack-self` does exactly this.

**Option 1 is disqualified by the reference's own stated safety rule**, which
this module inherits: *"this must never be the reason a tool call fails."*
Capturing a backtrace is not async-signal-safe — it allocates and takes loader
locks — so a thread interrupted while already inside one deadlocks. That
deadlock would be permanent, would hit the stuck thread, and would happen in
precisely the situation the diagnostic exists to explain. A profiler can accept
those odds across millions of samples. A diagnostic that fires once, on a
server, when something is already wrong, cannot.

Option 2 costs a **system C library** (`libunwind-ptrace`, e.g.
`libunwind-dev`) and permission to `ptrace`. That is a genuine departure from
this crate's feature policy, which has otherwise held a hard line — `symphonia`
was picked over ffmpeg bindings specifically so no system binary was needed,
and `rten` over the ONNX Runtime binding specifically so nothing is fetched at
run time. Taking a system library here is the first exception, and it should be
read as one.

## Decision

**Implement the dump with `rstack-self`, behind a `stack-dumps` Cargo feature
that is off by default and exists only on Linux.**

- Feature off (the default, and every non-Linux build): unchanged behaviour —
  identity and duration, no new dependency, nothing to install.
- Feature on, plus a binary that calls `watchdog::install_stack_dump_hook()`:
  every thread's stack, matching the reference.

Three things must hold for a dump to be attempted, and
`watchdog::stack_dumps_available()` is the single place that says so: the
feature, the platform, and the hook.

### The hook, and why it is an interlock rather than a formality

Capture works by re-executing `current_exe()` with a marker environment
variable set; the child sees the marker, `ptrace`s its parent, reports back,
and exits. That means **a binary that enables the feature but forgets the hook
would spawn a second copy of itself** — a second MCP server, behind the
operator's back, at the exact moment the first one is wedged.

So `install_stack_dump_hook()` sets a flag, and nothing is ever spawned unless
that flag is set. Forgetting the hook degrades to the feature-off behaviour;
it cannot misfire. `without_the_hook_no_dump_is_attempted` pins this, and it
passes in every test binary in the workspace — none of which can install the
hook, because a libtest harness has nowhere to put a first-thing-in-`main`
call.

A marker **environment variable, not an argv flag**: argv is the binary's
public interface, and a stray `--rstack-child` colliding with a real subcommand
or appearing in `--help` is a cost this diagnostic has no business imposing.

## Consequences

- **`libunwind-ptrace` becomes a build requirement — for those who opt in.**
  CI installs it in the `stack-dumps` step specifically, not globally, so the
  expense lands on the job that needs it and its absence keeps breaking any
  future step that forgets it.
- **And so does `liblzma`, which was not obvious.** `libunwind-ptrace.so`
  carries undefined `lzma_*` references. Whether they resolve depends on
  whether the local `libunwind.so` happens to declare `DT_NEEDED` on
  `liblzma.so.5` — some builds do, some do not. This first shipped green on a
  machine where they did and failed on a CI runner where they did not.
  `build.rs` now asks for `-llzma` explicitly, so the link never rests on that
  accident. `--allow-shlib-undefined` was the tempting alternative and is
  worse: it converts a build error into a crash the first time a dump reads a
  compressed debug section.
- **`libc` arrives transitively**, through `rstack-self`. ADR-0012 refused a
  *direct* `libc` dependency for a `kill(0)` probe that had a pure-`std`
  alternative. That reasoning is untouched: this has no `std` alternative, and
  the dependency exists only when the feature is on.
- **`ptrace` can be refused.** Ubuntu's default `yama.ptrace_scope=1` forbids
  tracing an ancestor; `rstack-self` handles it by calling
  `prctl(PR_SET_PTRACER, child)` in the parent. `ptrace_scope` of 2 or 3, a
  seccomp profile blocking `ptrace`, or a container without `CAP_SYS_PTRACE`
  will still refuse. A refused capture logs once and the report happens anyway
  — a failed dump never costs the line that would have been printed regardless.
- **Linux only.** The reference's `faulthandler` is cross-platform; this is
  not. macOS would need a different mechanism entirely (`task_for_pid`, and the
  code signing that goes with it). Windows already has no watchdog gap worth
  chasing here. Stated rather than papered over.
- **The dump stops every thread briefly.** That is the cost of the guarantee
  and is bounded by the capture; it is also strictly less disruptive than the
  status quo, which was attaching a debugger by hand.
- **`WatchdogStatus` is unchanged.** It keeps exactly the reference's
  `{enabled, threshold_seconds, calls_in_flight}`. Adding a `stack_dumps` field
  was considered and rejected: `remind_me_server_status` is a parity surface,
  and a build-time fact is not worth diverging it for. Availability is visible
  from the module docs and from the dump itself.

## Alternatives considered

- **A signal-handler unwinder (`pprof`-style, or hand-rolled with `libc`).**
  Rejected above on async-signal-safety. Worth restating because it is the
  cheaper-looking option and will look cheaper again to the next reader: it
  needs no system library and works on macOS too. It also can deadlock the
  thread it is diagnosing.
- **`rstack` rather than `rstack-self`.** Same library, but it traces *another*
  process, so it needs a separate always-running helper and the same `ptrace`
  permissions. More machinery, no more capability.
- **Shelling out to `py-spy`, or to `gdb`/`eu-stack`.**
  Rejected: it makes a diagnostic depend on a binary being installed at the
  moment of failure, which is exactly the situation where nobody has installed
  it. `rstack-self` fails closed at build time instead.
- **On by default.** Rejected. Every other optional feature here is off because
  it costs a dependency; this one also costs a system package and a
  privilege. A default that fails to build on a machine without
  `libunwind-dev` would be a bad trade for a diagnostic most deployments will
  never fire.
