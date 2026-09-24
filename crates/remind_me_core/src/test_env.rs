// Deliberate module-level allow: this one file is compiled into every test
// binary in the product (`remind_me_core`'s own unit tests via `mod`, every
// other test binary via `#[path]`), and a binary that only ever sets a var
// never calls `remove_var`. That "unused" is a property of the including
// binary, not dead code.
#![allow(dead_code)]

//! Test-only environment writes that cannot crash the test binary.
//!
//! **Every test in this product that changes a process env var goes through
//! [`set_var`] / [`remove_var`] here, never `std::env::set_var` /
//! `std::env::remove_var` directly.** `clippy.toml` at the product root
//! disallows the `std` pair so a direct call fails `clippy -D warnings`.
//!
//! ## Why
//!
//! `cargo test` runs tests on parallel threads. Rust's own `env::var` and
//! `env::set_var` share a lock inside `std`, so Rust code reading while a
//! test writes is safe. C code calling `getenv` takes no such lock, and
//! glibc's `setenv` may `realloc` the `environ` array when it adds a new
//! variable, freeing the array a concurrent C `getenv` is walking. That
//! read then segfaults the whole test binary.
//!
//! The C reader that actually bit us is SQLite. The first connection opened
//! in a process runs `sqlite3_initialize`, whose `unixTempFileInit` calls
//! `getenv("SQLITE_TMPDIR")` and `getenv("TMPDIR")`. A core dump of the
//! intermittent `image_import_test` SIGSEGV (CI, and about 1 run in 400
//! locally) showed exactly that frame racing another test's `set_var`.
//! SQLite reads those variables once per process and caches them, so the
//! race window is only "a test changes the environment before SQLite has
//! initialised".
//!
//! Hence the fix: before its first write, each binary forces SQLite to
//! initialise, by opening and dropping one in-memory database. SQLite
//! serialises initialisation behind its own mutex, so this blocks until any
//! initialisation already running on another thread has finished, and after
//! it nothing in SQLite calls `getenv` again. No test has to take a lock
//! just to be safe from a write it never sees.
//!
//! This covers the crash. It does not stop one test *seeing* a value
//! another test set: tests that depend on a variable's value still hold
//! their file's env lock, as before.
//!
//! No other C `getenv` caller has been seen in these test binaries. If one
//! turns up, prime it in [`prime_c_env_readers`] the same way.

use std::ffi::OsStr;
use std::sync::Once;

/// Sets the process env var `key` to `value`, after [`prime_c_env_readers`].
pub fn set_var(key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
    prime_c_env_readers();
    #[allow(clippy::disallowed_methods)]
    std::env::set_var(key, value);
}

/// Removes the process env var `key`, after [`prime_c_env_readers`].
pub fn remove_var(key: impl AsRef<OsStr>) {
    prime_c_env_readers();
    #[allow(clippy::disallowed_methods)]
    std::env::remove_var(key);
}

/// Runs, once per process, every C library initialisation known to read the
/// environment without `std`'s lock, so it finishes before any test writes.
fn prime_c_env_readers() {
    static PRIMED: Once = Once::new();
    PRIMED.call_once(|| {
        remind_me_core::Database::open_in_memory()
            .expect("opening an in-memory database to initialise SQLite");
    });
}
