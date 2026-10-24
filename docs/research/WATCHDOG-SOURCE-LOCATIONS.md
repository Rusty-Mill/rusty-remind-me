# Watchdog source-location diagnostic, 2026-10-24

## Scope and limits

Investigated PR #992 job `120918159220`, run `40108150969`, tested merge
`6aaf79c791c407d021e3a575bb21bfdb86e3f3e9` (`dabefe30` into `fe0a139`).
The reported stuck frame had its exact source file but no line. That output
omitted its PC, so **the precise original sampled instruction is unknown**.
Neither this investigation nor the fix attributes the failure to crypto.
The watchdog, fixture, assertion, feature workflow and lockfile are unchanged
between that merge and its base. The open-PR search found no equivalent fix.
The repair branch starts at current main `00b2c29ae929bc1616257880296e37bd4135672e`.

Natural sampling did not reproduce the failure on this machine. Controlled
instruction sampling reproduced a file-only frame and isolated an incorrect
PC adjustment. This is a demonstrated failure mechanism, not proof that the
original CI event sampled that same instruction.

## Environment and natural runs

- rustc `1.98.1 (48a229cea 2026-09-01)`, x86_64 Linux, AMD EPYC 7763.
- Debian 13, libunwind 1.8.1; libunwind/lzma development packages extracted
  into a workspace-local sysroot, using the installed runtime libraries.
- `RUSTFLAGS='-D warnings'`, both `CARGO_PROFILE_DEV_DEBUG` and
  `CARGO_PROFILE_TEST_DEBUG` set to `line-tables-only`.
- Diagnostic-only additions printed every named frame's raw PC and the
  fixture function's runtime address. The latter is removed from the repair.
- The initial batches shared a Cargo target directory across worktrees.
  Cargo reused artifacts; these preliminary counts characterize the identical
  affected source, but are not used as isolated revision evidence. Later
  comparisons below use separate target directories and copied executables.

| Revision / settings | Additional probe executions | Missing stuck-frame line |
| --- | ---: | ---: |
| main, incremental enabled (initial diagnostic) | 20 | 0 |
| tested merge, incremental enabled | 30 + 80 | 0 |
| base `fe0a139`, incremental enabled | 40 | 0 |
| tested merge, `CARGO_INCREMENTAL=0` (CI setting) | 40 + 400 | 0 |
| base `fe0a139`, `CARGO_INCREMENTAL=0` | 40 | 0 |
| main with floating counter, before PC fix | 80 | 0 |

The preliminary corresponding focused integration-test builds each passed all three
existing tests. The initial attempt before configuring the sysroot could not
build `unwind-sys`: `libunwind-ptrace.pc` was unavailable. No test ran in that
attempt. These are bounded diagnostic runs, not CI reruns.

For all natural integer-counter samples above, the sampled offset from
`remind_me_probe_stuck_frame` was one of `+0x6d`, `+0x79`, `+0x7f`, `+0x9c`.
None landed at the controlled failing offset below. Increasing the batch did
not establish the original CI failure's frequency on a different CPU.

The definitive isolated comparison, with incremental compilation disabled:

| Separate target directory | Probe runs | Failed strict assertions |
| --- | ---: | ---: |
| tested merge `6aaf79c`, original code plus IP/address output | 40 | 0 |
| base `fe0a139`, original code plus IP/address output | 40 | 0 |
| final repair on main `00b2c29ae` | 80 | 0 |

Each isolated merge/base focused integration suite passed 3/3. Every one of
these 160 probe outputs was checked for a named stuck frame with a positive
source line, the stuck thread, and the watchdog thread. The isolated merge
PC distribution was `+0x6d:34`, `+0x79:2`, `+0x7f:3`, `+0x9c:1`; base was
`+0x6d:31`, `+0x79:5`, `+0x7f:1`, `+0x9c:3`.

Reproduction commands (from each worktree, after making only the diagnostic
IP/address additions for the historical revisions):

```sh
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only
export RUSTFLAGS='-D warnings'
# Use a distinct directory per revision.
export CARGO_TARGET_DIR=/workspace/stack-merge-clean-target
cargo +1.98.1 test --locked -p remind_me_core --features stack-dumps \
  --test watchdog_stack_dump_test
# Copy watchdog_stack_probe from this target before running the batch.
# Run 40 copies (80 for final), with at most four concurrent processes.
```

## Instruction and dependency evidence

`objdump -d --disassemble=remind_me_probe_stuck_frame` and
`readelf --debug-dump=decodedline` on the tested-merge diagnostic executable
showed the checked integer increment has this layout (relative addresses):

```text
+0x94: mov 0x8(%rsp), %eax       DWARF line 0
+0x98: mov %eax, 0x24(%rsp)     DWARF line 42
+0x9c: jmp inner_loop           DWARF line 40
```

The actual symbolization chain is:

1. `rstack 0.3.3`, `src/imp/unwind.rs`, records the initial cursor before
   `cursor.step()`. Its signal flag comes from `cursor.is_signal_frame()`.
2. libunwind 1.8.1 `src/x86_64/init.h` initializes `sigcontext_format` to
   `X86_64_SCF_NONE`. `Gos-linux.c`'s `unw_is_signal_frame` tests that flag.
   An initially stopped ordinary thread is therefore not marked as a signal
   frame, even though its PC is a sampled instruction rather than a return PC.
3. `rstack-self 0.3.0`, `symbolicate_thread`, subtracts one unless that flag
   is set or the PC is zero, then calls `backtrace::resolve`.
4. **`backtrace 0.3.76` subtracts one again**, including for the address API:
   `ResolveWhat::address_or_ip` calls `adjust_ip` in `src/symbolize/mod.rs`.
   For an ordinary first frame this resolves sampled `PC - 2`.

Thus sampled `+0x98`, whose own row has line 42, resolves at `+0x96`, inside
line 0. Merely omitting rstack-self's adjustment still resolves at `+0x97`
and does not repair that boundary.

Sources: [rstack 0.3.3](https://docs.rs/crate/rstack/0.3.3/source/src/imp/unwind.rs),
[rstack-self 0.3.0](https://docs.rs/crate/rstack-self/0.3.0/source/src/lib.rs),
[backtrace 0.3.76](https://docs.rs/crate/backtrace/0.3.76/source/src/symbolize/mod.rs),
[libunwind initialization](https://github.com/libunwind/libunwind/blob/v1.8.1/src/x86_64/init.h),
[libunwind signal-frame classification](https://github.com/libunwind/libunwind/blob/v1.8.1/src/x86_64/Gos-linux.c).

## Controlled reproduction

This diagnostic used a **copy** of the tested-merge fixture executable. The
only source changes were the PC/address diagnostics and returning from main
after printing the capture instead of waiting to join the deliberately
permanently wedged worker. The CPU-bound function and its line tables stayed
unchanged. Nothing from this diagnostic mode ships.

For each target instruction, its first two bytes in `.text` were replaced by
x86 `eb fe` (jump to self). The unchanged line table then lets the real
ptrace stack dump sample a known, reproducible PC. The symbol's virtual
address came from `nm`, and `.text` virtual address/file offset from
`objdump -h`; file patch offset is `text_file_offset + symbol + delta - text_vaddr`.
The original bytes were checked (`8b44` at `+0x94`, `8944` at `+0x98`).

Before repair:

```text
symbol = 0x130550
sampled +0x94: named stuck frame, watchdog_stack_probe.rs:44
sampled +0x98: named stuck frame, watchdog_stack_probe.rs (NO LINE)
```

`addr2line -f -e probe 0x1305e4 0x1305e3 0x1305e8 0x1305e7`
reported respectively unknown line, 44, 42, unknown line. The `+0x94`
result also demonstrates that a nearby preceding line is not an exact source
location for the sampled instruction.

With the repaired watchdog resolver but the **original integer-counter
function** retained, a fresh separate-target control executable gave:

```text
symbol = 0x130920
sampled +0x94: named stuck frame, watchdog_stack_probe.rs (NO LINE)
sampled +0x98: named stuck frame, watchdog_stack_probe.rs:42
```

Both controlled executables exited successfully. This isolates the resolver
change from the fixture-counter change. The genuine line-0 case is preserved
honestly while the incorrectly adjusted, line-bearing case is repaired.

Two intermediate control builds were blocked by `--locked` because a current
main lockfile was copied into the historical workspace (unrelated historical
edges differed). Restoring its own lockfile and adding only its backtrace
edge corrected that setup. A shared-target control attempt then failed the
original-byte guard: Cargo had reused the floating-counter executable. That
attempt was discarded; the separate-target control above is authoritative.
No result from those setup attempts is counted as a passing diagnostic.

## Repair

The first frame of each thread is resolved from its raw sampled PC. Because
backtrace's address API subtracts one internally, the input is checked `PC + 1`.
Zero and overflow return no symbol; no source location is fabricated. A
function-entry regression fails without this compensation (no symbol found)
and passes with it; an unknown/overflow boundary test passes too.

This makes `backtrace`, already a transitive stack-dumps dependency, an
explicit optional Linux-only dependency. No registry package/version or
feature-off dependency tree is added. Caller frames retain rstack-self's
existing handling, including its signal classification; the repair does not
claim to fix every upstream caller/signal adjustment.

The fixture's counter is also `f64`: every integer through 20,000,000 is
exactly representable, preserving the iteration count while eliminating the
checked integer increment's line-0 spans in the steady hot loop. Arbitrary
real programs can still have compiler-generated instructions without source
lines. The output preserves file-only/unknown results honestly and now includes
the raw sampled IP for named frames as well as unnamed ones.

The source assertion is stronger: the named stuck frame itself must carry
`watchdog_stack_probe.rs:<positive integer>`. A line on another thread or a
file-only stuck frame cannot satisfy it.

## Final validation

All commands used Rust 1.98.1 and both line-table debug profile overrides.

- `cargo test --locked -p remind_me_core --features stack-dumps --no-fail-fast`:
  **1,737 passed**, no failed or ignored tests, across 118 test/doc suites.
  This includes the 3 integration tests and 2 new deterministic unit tests.
- `cargo clippy --locked -p remind_me_core --features stack-dumps --all-targets -- -D warnings`:
  passed.
- Final focused suite: 3/3; additional 80 probe outputs: all three strict
  assertions passed on every output.
- `cargo fmt --all -- --check`, `python3 .github/scripts/changes.py check`,
  and `git diff --check`: passed.
- Default normal dependency tree excludes both backtrace and rstack.

The first full-lane attempt stopped at the existing
`dbs_import::tests::a_dbs_page_imports_and_a_rerun_updates_what_changed`,
which creates scratch archives under the real home directory and encountered
`ReadOnlyFilesystem`; 427 other library tests passed. The final full run was
allowed to write those existing home-directory test fixtures and passed
without changing the test or home environment variable.

The function-entry unit regression was first run without compensating
backtrace's internal decrement: 1 passed / 1 failed (no symbol at the
function-entry lookup). With compensation: 2 passed / 0 failed.

Full workspace policy scripts were initially invoked without their required
metadata argument and did not validate. A subsequent offline full-workspace
metadata attempt could not download the uncached `ab_glyph 0.2.32`; no claim
of local workspace-wide policy validation is made. Those checks and exact-head
CI remain for the normal PR pipeline. No workflow reruns, provider activation,
credentials, bots or spending were used.
