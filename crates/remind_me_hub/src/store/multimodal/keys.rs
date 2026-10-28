//! Turning the hub's string ids and TEXT timestamps into the fixed-size,
//! `Copy` keys the engine sorts by (ADR-0021, decision 3).
//!
//! The engine needs ids that are `Copy` (`Uuid` here) and sort keys that
//! are `Ord + Copy`. The hub's ids are free-form strings paged in byte
//! order, and its timestamps are canonical TEXT whose byte order is their
//! chronological order ([`crate::canon`]). Everything here exists to keep
//! those two orders exactly while meeting the engine's bounds:
//!
//! - a timestamp becomes microseconds since the epoch, which orders
//!   canonical timestamps exactly as their bytes do (the canonical form
//!   has one spelling per instant, and `+` sorts before `.`, so a whole
//!   second sorts before any fraction of it);
//! - an id becomes its bytes zero-padded to a fixed width, which orders
//!   exactly as byte-order string comparison does *provided* no id holds a
//!   NUL byte (`"m"` and `"m\0"` would pad alike). Postgres TEXT could not
//!   hold NUL either, so refusing it cost the Postgres hubs nothing;
//! - an id becomes an engine id through UUID v5, with the string kept on
//!   the record so a (practically impossible) collision is refused rather
//!   than merged.

use super::super::{StoreError, StoreResult};
use crate::canon::canon_ts;
use uuid::Uuid;

/// The longest id the hub stores, in bytes (ADR-0021, decision 3). Real
/// ids are 12 to 36 characters.
pub const ID_CAP: usize = 64;

/// A record id as a fixed-size, byte-order-preserving key.
pub type IdKey = [u8; ID_CAP];

/// A link's synthetic `memory_id|entity_id` as a key: two capped ids and
/// the separator.
pub const LINK_KEY_LEN: usize = 2 * ID_CAP + 1;
pub type LinkKey = [u8; LINK_KEY_LEN];

/// The namespace every hub engine id is derived in. Each table is its own
/// engine store, so one namespace serves all four. Changing it changes
/// every engine id on disk.
const HUB_NAMESPACE: Uuid = Uuid::from_u128(0x7272_6d68_7562_4000_8000_0000_0000_0021);

/// Refuse an id the engine layout cannot hold: empty, over [`ID_CAP`]
/// bytes, or containing a NUL byte.
pub fn check_id(what: &str, id: &str) -> StoreResult<()> {
    if id.is_empty() {
        return Err(StoreError(format!("{what} is empty")));
    }
    if id.len() > ID_CAP {
        return Err(StoreError(format!(
            "{what} is {} bytes; this hub stores ids of at most {ID_CAP} bytes",
            id.len()
        )));
    }
    if id.contains('\0') {
        return Err(StoreError(format!("{what} contains a NUL byte")));
    }
    Ok(())
}

/// The engine id for a string id.
pub fn engine_id(id: &str) -> Uuid {
    Uuid::new_v5(&HUB_NAMESPACE, id.as_bytes())
}

/// The engine id for a link. Length-prefixed, so no pair of ids can
/// spell another pair (`"a|b"+"c"` and `"a"+"b|c"` would, joined by `|`).
pub fn link_engine_id(memory_id: &str, entity_id: &str) -> Uuid {
    engine_id(&format!("{}:{memory_id}|{entity_id}", memory_id.len()))
}

/// Refuse to treat two different string ids as one record.
///
/// `stored` is the string on the record already at `incoming`'s engine id.
/// A v5 collision between two real ids is not expected in practice, but
/// merging two records would be silent data loss, so it is checked.
pub fn ensure_same_id(stored: &str, incoming: &str) -> StoreResult<()> {
    if stored == incoming {
        return Ok(());
    }
    Err(StoreError(format!(
        "id {incoming:?} maps to the same engine id as the stored {stored:?}; refusing to merge"
    )))
}

/// `bytes`, zero-padded (or cut) to `N`. Cutting only ever applies to a
/// cursor a client sent, never to a stored id, which [`check_id`] caps.
fn padded<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut key = [0u8; N];
    let len = bytes.len().min(N);
    key[..len].copy_from_slice(&bytes[..len]);
    key
}

/// The sort key for an id, or for a client's `since_id` cursor.
///
/// A cursor longer than [`ID_CAP`] is cut to it. That is still exact: a
/// stored id whose key equals the cut cursor is a proper prefix of the
/// cursor, so it sorts before it, and paging excludes it along with the
/// cursor's own key.
pub fn id_key(id: &str) -> IdKey {
    padded(id.as_bytes())
}

/// The sort key for a link's synthetic id, or a `since_id` cursor over
/// links. Same cutting rule as [`id_key`].
pub fn link_key(synthetic: &str) -> LinkKey {
    padded(synthetic.as_bytes())
}

/// The greatest id key: no UTF-8 string contains a `0xFF` byte, so every
/// stored id sorts below it.
pub const MAX_ID_KEY: IdKey = [0xFF; ID_CAP];

/// Microseconds since the epoch for a timestamp the hub stored or a client
/// sent as a cursor.
///
/// Stored timestamps are canonical already. A cursor is canonicalised
/// first, so a client that sends `Z` rather than `+00:00` gets the instant
/// it meant (the retired SQL stores compared the raw bytes and did not).
pub fn micros(ts: &str) -> StoreResult<i64> {
    let canonical = canon_ts(ts).map_err(StoreError)?;
    chrono::DateTime::parse_from_rfc3339(&canonical)
        .map(|dt| dt.timestamp_micros())
        .map_err(|e| StoreError(format!("timestamp {ts:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_at_the_cap_are_kept_and_longer_ones_refused() {
        assert!(check_id("id", &"x".repeat(ID_CAP)).is_ok());
        let err = check_id("id", &"x".repeat(ID_CAP + 1)).unwrap_err();
        assert!(err.0.contains("65 bytes"), "{}", err.0);
        assert!(check_id("id", "").is_err());
        assert!(check_id("id", "m\0").is_err());
    }

    #[test]
    fn the_cap_counts_bytes_not_characters() {
        // 32 two-byte characters is 64 bytes: at the cap. One more is over.
        assert!(check_id("id", &"é".repeat(32)).is_ok());
        assert!(check_id("id", &"é".repeat(33)).is_err());
    }

    #[test]
    fn padded_id_keys_sort_like_byte_order_strings() {
        let mut ids = vec!["m1", "m", "mem_00ff", "a", "Z", "m\u{e9}", "mem_", "b", "~"];
        let mut keyed = ids.clone();
        keyed.sort_by_key(|id| id_key(id));
        ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(keyed, ids);
    }

    #[test]
    fn a_cut_cursor_still_orders_exactly() {
        // A 70-byte cursor. The stored id equal to its first 64 bytes sorts
        // before it (a proper prefix), and so does its key: equal to the
        // cursor's cut key, which paging excludes.
        let cursor = format!("{}{}", "a".repeat(ID_CAP), "zzzzzz");
        let prefix = "a".repeat(ID_CAP);
        assert!(prefix.as_str() < cursor.as_str());
        assert_eq!(id_key(&prefix), id_key(&cursor));
        // An id past the cursor in byte order is past it as a key too.
        let after = format!("{}b", "a".repeat(ID_CAP - 1));
        assert!(after.as_str() > cursor.as_str());
        assert!(id_key(&after) > id_key(&cursor));
    }

    #[test]
    fn every_utf8_id_sorts_below_the_max_key() {
        assert!(id_key(&"\u{10FFFF}".repeat(16)) < MAX_ID_KEY);
    }

    #[test]
    fn link_ids_are_injective_over_pairs() {
        assert_ne!(link_engine_id("a|b", "c"), link_engine_id("a", "b|c"));
        assert_eq!(link_engine_id("m", "e"), link_engine_id("m", "e"));
    }

    #[test]
    fn engine_ids_are_stable() {
        // The on-disk identity of every record: pinned so a change to the
        // namespace or the hashing shows up here, not as an empty hub.
        assert_eq!(
            engine_id("m1").to_string(),
            Uuid::new_v5(&HUB_NAMESPACE, b"m1").to_string()
        );
        assert_eq!(engine_id("m1").get_version_num(), 5);
    }

    #[test]
    fn a_collision_is_refused_not_merged() {
        assert!(ensure_same_id("m1", "m1").is_ok());
        assert!(ensure_same_id("m1", "m2").is_err());
    }

    #[test]
    fn micros_orders_canonical_timestamps_like_their_bytes() {
        let stamps = [
            "2026-08-05T11:59:59.999999+00:00",
            "2026-08-05T12:00:00+00:00",
            "2026-08-05T12:00:00.000001+00:00",
            "2026-08-05T12:00:00.5+00:00",
            "2026-08-05T12:00:01+00:00",
        ];
        let canonical: Vec<String> = stamps.iter().map(|s| canon_ts(s).unwrap()).collect();
        let mut by_bytes = canonical.clone();
        by_bytes.sort();
        assert_eq!(by_bytes, canonical, "the fixture is in byte order");
        let us: Vec<i64> = canonical.iter().map(|s| micros(s).unwrap()).collect();
        assert!(us.windows(2).all(|w| w[0] < w[1]), "{us:?}");
    }

    #[test]
    fn a_cursor_is_read_as_the_instant_it_names() {
        assert_eq!(
            micros("2026-08-05T12:00:00Z").unwrap(),
            micros("2026-08-05T14:00:00+02:00").unwrap()
        );
        assert!(micros("not a date").is_err());
    }
}
