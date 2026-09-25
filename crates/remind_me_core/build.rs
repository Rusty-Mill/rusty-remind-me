//! Link `liblzma` when the `stack-dumps` feature is on, and nothing otherwise.
//!
//! This exists because of a genuinely confusing failure. `rstack-self` links
//! `libunwind-ptrace`, whose Ubuntu build carries undefined references to
//! `lzma_*` (it reads XZ-compressed debug sections). Whether the link
//! *succeeds* then depends on something neither this crate nor `rstack-self`
//! controls: some builds of `libunwind.so` declare `DT_NEEDED` on
//! `liblzma.so.5`, which lets the linker resolve those symbols by accident,
//! and some do not.
//!
//! That difference is invisible until it bites. The feature built and its
//! tests passed on a machine whose `libunwind.so` declared the dependency, and
//! failed on a CI runner whose did not, with:
//!
//! ```text
//! rust-lld: error: undefined reference: lzma_stream_buffer_decode
//!   >>> referenced by /usr/lib/x86_64-linux-gnu/libunwind-ptrace.so
//!       (disallowed by --no-allow-shlib-undefined)
//! ```
//!
//! Asking for `lzma` explicitly makes the requirement real rather than
//! incidental, so the link behaves the same on both. The alternative --
//! `--allow-shlib-undefined` -- would silence the error and leave the symbols
//! genuinely unresolved, turning a build failure into a crash the first time a
//! dump touches a compressed debug section. Worse, and later.
//!
//! `liblzma-dev` (for the `liblzma.so` symlink) is therefore part of what the
//! `stack-dumps` feature needs, alongside `libunwind-dev`. Both are listed in
//! the feature's comment in `Cargo.toml` and installed by the CI step.

fn main() {
    // Cargo re-runs a build script on any change by default, which would
    // invalidate the whole crate's cache constantly. This one depends on
    // nothing but its own source.
    println!("cargo:rerun-if-changed=build.rs");

    let stack_dumps = std::env::var_os("CARGO_FEATURE_STACK_DUMPS").is_some();
    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    if stack_dumps && linux {
        println!("cargo:rustc-link-lib=lzma");
    }
}
