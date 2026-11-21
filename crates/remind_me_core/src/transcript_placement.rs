//! Where a resent session transcript's messages fall in the recorded dialog.
//!
//! A recapture gets the whole session so far, an older shorter copy, or only
//! the tail of a session too long to send whole. Before any turn is added to
//! the read-only record (#1616), each message is placed: already held, or
//! new. Pure: no store, no I/O.
//!
//! Placement prefers a stable id (each Claude Code message carries a `uuid`)
//! and falls back to text only when an overlap can be shown to be unique.
//! When neither works the caller keeps the input whole under a flag, so no
//! turn is skipped silently and no duplicate goes unmarked.

use std::collections::HashSet;

/// Separator between rendered messages in a stored dialog.
pub const MESSAGE_SEPARATOR: &str = "\n\n";

/// How the new messages of a transcript were found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Every message is already recorded: nothing to add.
    Held,
    /// The messages at these indexes are new, in transcript order.
    New { indexes: Vec<usize>, by: Method },
    /// No placement could be shown; the caller keeps every message, flagged.
    Unplaced(Reason),
}

/// What a [`Placement::New`] rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Message ids the record lists.
    Id,
    /// A text overlap that occurs once in the record, at its end.
    Overlap,
}

/// Why a transcript could not be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No recorded turn was found in it.
    NoOverlap,
    /// More than one place fits, or the fitting overlap occurs more than
    /// once in the record.
    Ambiguous,
}

impl Placement {
    /// The flag stored on a continuation row.
    pub fn label(&self) -> &'static str {
        match self {
            Placement::Held => "held",
            Placement::New { by: Method::Id, .. } => "id",
            Placement::New {
                by: Method::Overlap,
                ..
            } => "overlap",
            Placement::Unplaced(Reason::NoOverlap) => "unplaced_no_overlap",
            Placement::Unplaced(Reason::Ambiguous) => "unplaced_ambiguous",
        }
    }
}

/// What the record holds.
pub struct Record<'a> {
    /// The dialog rows' text, joined with [`MESSAGE_SEPARATOR`].
    pub text: &'a str,
    /// Every message id the rows hold, when every row lists its ids.
    pub ids: Option<&'a HashSet<String>>,
}

/// A transcript's messages as the record would store them.
pub struct Incoming<'a> {
    /// Each message rendered and scrubbed.
    pub blocks: &'a [String],
    /// Each message's id, when every message has one.
    pub ids: Option<&'a [String]>,
}

/// Place `incoming` against `record`.
pub fn place(record: &Record<'_>, incoming: &Incoming<'_>) -> Placement {
    if incoming.blocks.is_empty() {
        return Placement::Held;
    }
    if let (Some(held), Some(ids)) = (record.ids, incoming.ids) {
        return by_id(held, ids);
    }
    by_text(record.text.trim_end(), incoming.blocks)
}

fn by_id(held: &HashSet<String>, ids: &[String]) -> Placement {
    let indexes: Vec<usize> = (0..ids.len())
        .filter(|&i| !held.contains(&ids[i]))
        .collect();
    if indexes.is_empty() {
        return Placement::Held;
    }
    Placement::New {
        indexes,
        by: Method::Id,
    }
}

fn by_text(record: &str, blocks: &[String]) -> Placement {
    // A copy the record already holds whole, such as an older shorter one.
    if occurrences(record, &blocks.join(MESSAGE_SEPARATOR)) > 0 {
        return Placement::Held;
    }
    // Where the transcript's first j+1 messages are exactly how the record
    // ends. Placed only when one such j exists and its text occurs nowhere
    // else in the record; any other fit makes the placement ambiguous.
    let fits: Vec<usize> = (0..blocks.len())
        .filter(|&j| record.ends_with(blocks[..=j].join(MESSAGE_SEPARATOR).as_str()))
        .collect();
    match fits.as_slice() {
        [] => Placement::Unplaced(Reason::NoOverlap),
        [j] if occurrences(record, &blocks[..=*j].join(MESSAGE_SEPARATOR)) == 1 => Placement::New {
            indexes: (j + 1..blocks.len()).collect(),
            by: Method::Overlap,
        },
        _ => Placement::Unplaced(Reason::Ambiguous),
    }
}

/// How many times `needle` occurs in `haystack`, overlapping occurrences
/// included; `0` for an empty needle.
fn occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        count += 1;
        let start = from + at;
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| format!("**user:** {t}")).collect()
    }

    fn by_text_of(record: &[&str], sent: &[&str]) -> Placement {
        let text = blocks(record).join(MESSAGE_SEPARATOR);
        let sent = blocks(sent);
        place(
            &Record {
                text: &text,
                ids: None,
            },
            &Incoming {
                blocks: &sent,
                ids: None,
            },
        )
    }

    fn new_at(indexes: impl IntoIterator<Item = usize>) -> Placement {
        Placement::New {
            indexes: indexes.into_iter().collect(),
            by: Method::Overlap,
        }
    }

    #[test]
    fn text_places_a_grown_copy_and_a_mid_session_tail() {
        let record = ["a", "b", "c", "d"];
        assert_eq!(by_text_of(&record, &["a", "b", "c", "d", "e"]), new_at([4]));
        assert_eq!(by_text_of(&record, &["c", "d", "e", "f"]), new_at([2, 3]));
    }

    #[test]
    fn text_recognises_a_copy_the_record_already_holds() {
        let record = ["a", "b", "c", "d"];
        assert_eq!(by_text_of(&record, &record), Placement::Held);
        // An older, shorter copy sent again.
        assert_eq!(by_text_of(&record, &["a", "b"]), Placement::Held);
        assert_eq!(by_text_of(&record, &["b", "c"]), Placement::Held);
    }

    #[test]
    fn text_with_no_recorded_turn_is_unplaced() {
        assert_eq!(
            by_text_of(&["a", "b"], &["x", "y"]),
            Placement::Unplaced(Reason::NoOverlap)
        );
    }

    #[test]
    fn repeated_short_turns_are_ambiguous_not_guessed() {
        // "yes" ends the record and occurs in it twice: it shows nothing.
        assert_eq!(
            by_text_of(&["q1", "yes", "q2", "yes"], &["yes", "q3"]),
            Placement::Unplaced(Reason::Ambiguous)
        );
        // Two prefixes fit the end of a periodic record.
        assert_eq!(
            by_text_of(&["yes", "yes"], &["yes", "yes", "yes", "new"]),
            Placement::Unplaced(Reason::Ambiguous)
        );
        // A longer run that occurs once still places.
        assert_eq!(
            by_text_of(&["q1", "yes", "q2", "yes"], &["q2", "yes", "q3"]),
            new_at([2])
        );
    }

    #[test]
    fn ids_place_exactly_whatever_the_text_repeats() {
        let held: HashSet<String> = ["u1", "u2", "u3"].map(String::from).into();
        let sent = blocks(&["yes", "yes", "yes", "yes"]);
        let place_ids = |ids: &[&str]| {
            let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
            place(
                &Record {
                    text: "",
                    ids: Some(&held),
                },
                &Incoming {
                    blocks: &sent[..ids.len()],
                    ids: Some(&ids),
                },
            )
        };
        assert_eq!(place_ids(&["u1", "u2"]), Placement::Held);
        assert_eq!(
            place_ids(&["u2", "u3", "u4", "u5"]),
            Placement::New {
                indexes: vec![2, 3],
                by: Method::Id
            }
        );
    }

    #[test]
    fn occurrences_counts_overlapping_matches() {
        assert_eq!(occurrences("aaa", "aa"), 2);
        assert_eq!(occurrences("abc", ""), 0);
        assert_eq!(occurrences("é é", "é"), 2);
    }
}
