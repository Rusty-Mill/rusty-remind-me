//! The store-level settings a client and the daemon must agree on.
//!
//! A daemon takes its settings from the environment of the client that
//! started it. A later client configured differently (another sync secret,
//! another embedding backend, another watch directory) would otherwise be
//! served under the first client's settings without knowing. So each client
//! sends a fingerprint of its own `REMIND_ME_*` settings, and on any
//! difference it does not use the daemon: it runs in-process as before
//! (ADR-0023 §2).
//!
//! Values are sent as SHA-256 digests, never in the clear: the set includes
//! secrets, and a digest is enough to compare. The comparison is deliberately
//! wide, every `REMIND_ME_*` variable except [`EXCLUDED`]: a variable wrongly
//! compared only costs a fallback, while one wrongly skipped would be served
//! under the wrong value.

use std::collections::BTreeMap;

/// `REMIND_ME_*` variables that are not the store's to agree on.
const EXCLUDED: &[&str] = &[
    // Chooses whether to use the daemon at all.
    super::ENABLE_ENV,
    // Locate the store, which already picks the daemon; spelling the same
    // path two ways must not count as a difference.
    crate::db::DB_PATH_ENV,
    "REMIND_ME_MCP_DIR",
    // Set only in the watchdog's own tracer child.
    "REMIND_ME_WATCHDOG_STACK_CHILD",
    // Read only by the daemon, to time its own compaction: no client
    // behaves differently for it.
    crate::compaction::COMPACT_INTERVAL_ENV,
];

/// Prefixes of variables that configure the client process itself: `remote`
/// binds its own listener and holds its own connector token.
const EXCLUDED_PREFIXES: &[&str] = &["REMIND_ME_REMOTE_"];

/// Name to SHA-256 digest of value, for every compared variable that is set.
pub type Fingerprint = BTreeMap<String, String>;

/// Whether `name` is compared.
fn compared(name: &str) -> bool {
    name.starts_with("REMIND_ME_")
        && !super::session::SESSION_VARS.contains(&name)
        && !EXCLUDED.contains(&name)
        && !EXCLUDED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// This process's fingerprint.
pub fn fingerprint() -> Fingerprint {
    fingerprint_of(std::env::vars())
}

/// [`fingerprint`] over the given variables, for tests.
pub fn fingerprint_of(vars: impl IntoIterator<Item = (String, String)>) -> Fingerprint {
    vars.into_iter()
        .filter(|(name, _)| compared(name))
        .map(|(name, value)| (name, sha256::digest(value)))
        .collect()
}

/// Names whose value differs, or that one side sets and the other does not.
pub fn differences(ours: &Fingerprint, theirs: &Fingerprint) -> Vec<String> {
    let names: std::collections::BTreeSet<&String> = ours
        .keys()
        .chain(theirs.keys())
        .filter(|name| ours.get(*name) != theirs.get(*name))
        .collect();
    names.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(pairs: &[(&str, &str)]) -> Fingerprint {
        fingerprint_of(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn only_store_settings_are_compared() {
        let got = fp(&[
            ("REMIND_ME_SYNC_SECRET", "s"),
            ("REMIND_ME_CLIENT", "cursor"),
            ("REMIND_ME_DAEMON", "1"),
            ("REMIND_ME_DB_PATH", "~/x.db"),
            ("REMIND_ME_REMOTE_PORT", "8765"),
            ("HOME", "/root"),
        ]);
        assert_eq!(got.keys().collect::<Vec<_>>(), ["REMIND_ME_SYNC_SECRET"]);
    }

    #[test]
    fn values_are_digested_not_sent() {
        let got = fp(&[("REMIND_ME_SYNC_SECRET", "hunter2")]);
        assert!(!got["REMIND_ME_SYNC_SECRET"].contains("hunter2"));
        assert_eq!(got["REMIND_ME_SYNC_SECRET"].len(), 64);
    }

    #[test]
    fn differences_name_changed_added_and_removed_settings() {
        let ours = fp(&[("REMIND_ME_A", "1"), ("REMIND_ME_B", "2")]);
        let theirs = fp(&[("REMIND_ME_A", "2"), ("REMIND_ME_C", "x")]);
        assert_eq!(
            differences(&ours, &theirs),
            ["REMIND_ME_A", "REMIND_ME_B", "REMIND_ME_C"]
        );
        assert!(differences(&ours, &ours).is_empty());
    }
}
