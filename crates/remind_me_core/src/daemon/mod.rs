//! The store daemon: one process owns the data and every other process is its
//! client (ADR-0023 §2).
//!
//! Opt-in while the process model beds in: [`enabled`] is off unless
//! `REMIND_ME_DAEMON` says otherwise, and every client falls back to opening
//! the store in-process, as before, whenever it cannot use the daemon.

pub mod client;
pub mod endpoint;
pub mod ops;
pub mod server;
pub mod session;
pub mod settings;
pub mod wire;

/// Set to `1` (or `true`, `yes`, `on`) to route this client through the daemon.
pub const ENABLE_ENV: &str = "REMIND_ME_DAEMON";

/// Whether this process should use the daemon.
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENABLE_ENV).ok().as_deref())
}

/// [`enabled`] with the value injected, for tests.
pub fn enabled_from(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_enables_it() {
        for on in ["1", "true", "YES", " on "] {
            assert!(enabled_from(Some(on)), "{on}");
        }
        for off in [None, Some(""), Some("0"), Some("false"), Some("daemon")] {
            assert!(!enabled_from(off), "{off:?}");
        }
    }
}
