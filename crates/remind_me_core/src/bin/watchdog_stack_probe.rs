//! Test fixture for the `stack-dumps` watchdog. Not a shipped tool.
//!
//! `watchdog_stack_dump_test` needs a *real* binary to trace: the capture path
//! re-executes `current_exe()`, and a libtest harness cannot host the
//! first-thing-in-`main` hook that requires. So the fixture is a binary, gated
//! on the same feature it exercises, and the integration test finds it through
//! `CARGO_BIN_EXE_watchdog_stack_probe`.
//!
//! It wedges a thread in a CPU-bound loop -- the reference's motivating
//! incident, a runaway query pegging a core -- and holds a call open past a
//! deliberately tiny threshold, so the watchdog fires and dumps.

use remind_me_core::watchdog::{self, Watchdog};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The frame the test greps for. `#[inline(never)]` and `no_mangle` because a
/// symbol the optimiser folded away would make this a test of nothing.
///
/// The inner loop's shape is load-bearing, and was arrived at by measurement.
///
/// A dump samples whatever instruction the thread happens to be on. In a debug
/// build `wrapping_mul`, `stop.load`, and even `0..n` iteration are all real
/// calls, so the sample usually landed in a `std` helper — and the unwinder
/// does not reliably recover the caller from there, which made asserting on
/// this function's name flaky (it resolved in roughly 1 run in 5).
///
/// So the hot loop is float arithmetic and a raw counter: both compile to
/// inline instructions with no call, even unoptimised, which keeps the sampled
/// PC inside this frame. The counter is also floating point: a checked integer
/// increment emits compiler-generated, line-0 instruction spans in a debug
/// build. A sample there can name this function and file without a source line.
/// Every integer through 20,000,000 is exactly representable in `f64`, so this
/// still performs the same bounded number of iterations without overflow code.
/// `stop` is checked only between long stretches. That
/// is also a truer model of the incident this feature exists for — a runaway
/// query pegging a core, not a spin on a flag.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn remind_me_probe_stuck_frame(stop: &AtomicBool) {
    let mut x = 1.0f64;
    while !stop.load(Ordering::Relaxed) {
        let mut i = 0.0f64;
        while i < 20_000_000.0 {
            x = x * 1.000_000_1 + 1.0;
            i += 1.0;
        }
        std::hint::black_box(x);
    }
}

fn main() {
    // First thing, before anything else -- see `install_stack_dump_hook`.
    watchdog::install_stack_dump_hook();

    if !watchdog::stack_dumps_available() {
        eprintln!("probe: stack dumps unavailable");
        std::process::exit(2);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("probe-stuck".to_string())
        .spawn(move || remind_me_probe_stuck_frame(&worker_stop))
        .expect("spawn the stuck thread");

    // Report to stdout so the test reads one stream. The default sink writes
    // to stderr, which the test would otherwise have to interleave.
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let sink: watchdog::Sink = Arc::new(move |stuck: &watchdog::StuckCall| {
        let _ = tx
            .lock()
            .expect("sink lock")
            .send(stuck.stacks.clone().unwrap_or_default());
    });

    let watchdog = Watchdog::new(Some(Duration::from_millis(100)), sink);
    let guard = watchdog.arm("remind_me_probe");

    let stacks = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the watchdog should report the held call");

    drop(guard);
    stop.store(true, Ordering::Relaxed);
    worker.join().expect("join the stuck thread");

    println!("{stacks}");
}
