//! The store daemon: one process owns the data and every other process is its
//! client (ADR-0023 §2).
//!
//! On by default (ADR-0023, phase 2b): [`enabled`] is on unless
//! `REMIND_ME_DAEMON` turns it off, and every client falls back to opening
//! the store in-process, as before, whenever it cannot use the daemon.

pub mod client;
pub mod endpoint;
pub mod ops;
pub mod server;
pub mod session;
pub mod settings;
pub mod wire;

/// Set to `0` (or `false`, `no`, `off`) to open the store in this process
/// rather than through the daemon.
pub const ENABLE_ENV: &str = "REMIND_ME_DAEMON";

/// Whether this process should use the daemon.
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENABLE_ENV).ok().as_deref())
}

/// [`enabled`] with the value injected, for tests.
pub fn enabled_from(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_no_disables_it() {
        for on in [None, Some(""), Some("1"), Some("true"), Some("daemon")] {
            assert!(enabled_from(on), "{on:?}");
        }
        for off in ["0", "false", "NO", " off "] {
            assert!(!enabled_from(Some(off)), "{off}");
        }
    }
}
