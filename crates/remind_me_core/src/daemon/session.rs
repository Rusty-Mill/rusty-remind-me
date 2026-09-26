//! Settings that belong to one client session rather than to the store.
//!
//! Before the daemon, every MCP client ran its own process and read its own
//! environment, so `REMIND_ME_CLIENT`, `REMIND_ME_DEFAULT_RESPONSE_FORMAT` and
//! `REMIND_ME_TOOL_PROFILE` were per-client by construction. One daemon serves
//! every client, so those three travel with each connection instead
//! (ADR-0023 §2) and are read through [`var`], which prefers the calling
//! connection's value to the daemon's own environment.
//!
//! The daemon runs each connection on its own thread, so the session is a
//! thread-local: [`enter`] sets it for the rest of that thread's life. Outside
//! a daemon connection nothing is entered and [`var`] is plain
//! `std::env::var`.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// The variables a client's session carries to the daemon.
pub const SESSION_VARS: [&str; 3] = [
    crate::sync::CLIENT_ENV,
    "REMIND_ME_DEFAULT_RESPONSE_FORMAT",
    crate::tool_profiles::TOOL_PROFILE_ENV,
];

/// One connection's session: its values of [`SESSION_VARS`], plus the client
/// identity its MCP `initialize` reported, once it has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Only [`SESSION_VARS`] entries the client had set.
    pub vars: BTreeMap<String, String>,
    #[serde(skip)]
    pub handshake_client: Option<String>,
}

impl Session {
    /// The calling process's own values of [`SESSION_VARS`].
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Session::from_env`] with the environment injected, for tests.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let vars = SESSION_VARS
            .iter()
            .filter_map(|name| get(name).map(|value| (name.to_string(), value)))
            .collect();
        Self {
            vars,
            handshake_client: None,
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Make `session` this thread's session for the rest of its life.
pub fn enter(session: Session) {
    CURRENT.with(|slot| *slot.borrow_mut() = Some(session));
}

/// Whether this thread is serving a daemon connection.
pub fn is_active() -> bool {
    CURRENT.with(|slot| slot.borrow().is_some())
}

/// `name` as the current session sees it.
///
/// In a session, a [`SESSION_VARS`] entry comes from the client, and is
/// `None` when the client had it unset even if the daemon's environment has
/// it. Anything else, and everything outside a session, is the process
/// environment.
pub fn var(name: &str) -> Option<String> {
    if !SESSION_VARS.contains(&name) {
        return std::env::var(name).ok();
    }
    CURRENT.with(|slot| match slot.borrow().as_ref() {
        Some(session) => session.vars.get(name).cloned(),
        None => std::env::var(name).ok(),
    })
}

/// Record the handshake client in the current session. Returns `false`, and
/// records nothing, outside a session.
pub fn set_handshake_client(identity: Option<String>) -> bool {
    CURRENT.with(|slot| match slot.borrow_mut().as_mut() {
        Some(session) => {
            session.handshake_client = identity;
            true
        }
        None => false,
    })
}

/// The current session's handshake client: `Some(None)` in a session whose
/// client has not said who it is, `None` outside a session.
pub fn handshake_client() -> Option<Option<String>> {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|session| session.handshake_client.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pairs: &[(&str, &str)]) -> Session {
        Session {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            handshake_client: None,
        }
    }

    #[test]
    fn from_lookup_keeps_only_session_vars_that_are_set() {
        let got = Session::from_lookup(|name| match name {
            "REMIND_ME_CLIENT" => Some("cursor".into()),
            "REMIND_ME_SYNC_SECRET" => Some("s".into()),
            _ => None,
        });
        assert_eq!(got, session(&[("REMIND_ME_CLIENT", "cursor")]));
    }

    #[test]
    fn a_session_answers_for_its_own_vars_on_its_own_thread() {
        std::thread::spawn(|| {
            assert!(!is_active());
            enter(session(&[("REMIND_ME_TOOL_PROFILE", "core")]));
            assert!(is_active());
            assert_eq!(var("REMIND_ME_TOOL_PROFILE").as_deref(), Some("core"));
            // Unset in the session reads as unset, whatever the daemon has.
            assert_eq!(var("REMIND_ME_CLIENT"), None);
        })
        .join()
        .unwrap();
        // Another thread never sees it.
        std::thread::spawn(|| assert!(!is_active())).join().unwrap();
    }

    #[test]
    fn the_handshake_client_lives_in_the_session() {
        std::thread::spawn(|| {
            assert!(!set_handshake_client(Some("x".into())));
            assert_eq!(handshake_client(), None);
            enter(Session::default());
            assert_eq!(handshake_client(), Some(None));
            assert!(set_handshake_client(Some("claude-code/2".into())));
            assert_eq!(handshake_client(), Some(Some("claude-code/2".into())));
        })
        .join()
        .unwrap();
    }
}
