//! The `stack-dumps` watchdog, exercised for real rather than type-checked.
//!
//! The whole claim of the feature is that a thread wedged in synchronous
//! CPU-bound code still gets dumped — the reference's motivating incident, and
//! the thing an in-process approach cannot do. A test that only asserted "some
//! text came back" would pass just as happily against a dump of the watchdog
//! thread sitting in `read`, which would be worthless. So these assert on a
//! named frame belonging to the *stuck* thread specifically.
//!
//! Runs against `watchdog_stack_probe` (see `src/bin/`), because capture works
//! by re-executing `current_exe()` and a libtest harness cannot host the
//! first-thing-in-`main` hook that needs.
#![cfg(all(target_os = "linux", feature = "stack-dumps"))]

use std::process::Command;

/// Generous: the probe's own threshold is 100ms, but this spawns a process
/// that ptraces another process on a machine already running the rest of the
/// suite in parallel. A tight bound here would buy nothing but flakes.
const PROBE_TIMEOUT_SECS: u64 = 120;

fn run_probe() -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_watchdog_stack_probe"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the watchdog stack probe");

    // Wait with a deadline. A hung probe should fail this test, not the whole
    // suite's wall clock -- and `wait_timeout` is a dependency this crate has
    // no other reason to carry, so poll.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(PROBE_TIMEOUT_SECS);
    loop {
        match child.try_wait().expect("poll the probe") {
            Some(status) => {
                let out = child.wait_with_output().expect("collect probe output");
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                assert!(
                    status.success(),
                    "probe exited with {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                );
                return stdout;
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("the stack-dump probe did not finish within {PROBE_TIMEOUT_SECS}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

#[test]
fn a_thread_wedged_in_cpu_bound_code_appears_in_the_dump() {
    let dump = run_probe();

    assert!(
        dump.contains("remind_me_probe_stuck_frame"),
        "the dump must name the frame the stuck thread is actually in -- \
         this is the guarantee the whole feature exists for.\n\n{dump}"
    );
}

#[test]
fn the_dump_covers_every_thread_not_just_the_stuck_one() {
    let dump = run_probe();

    // Linux truncates thread names to 15 bytes, so match the prefix the
    // kernel actually keeps rather than the name as spelled at spawn.
    assert!(
        dump.contains("probe-stuck"),
        "the stuck thread should be named in the dump\n\n{dump}"
    );
    assert!(
        dump.contains("remind-me-watch"),
        "the watchdog's own thread should appear too -- the reference dumps \
         *every* thread, and a dump of only the interesting one would be a \
         different, weaker promise\n\n{dump}"
    );
}

#[test]
fn frames_carry_source_locations_not_just_addresses() {
    let dump = run_probe();

    // The point of unwinding rather than printing raw instruction pointers.
    // Asserted against the probe's own file, which is guaranteed to have
    // debug info in a test build, rather than against std or libc.
    assert!(
        dump.contains("watchdog_stack_probe.rs:"),
        "frames should resolve to file:line, not bare addresses\n\n{dump}"
    );
}
