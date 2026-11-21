//! Where a resent session transcript's messages fall in the recorded dialog.
//!
//! A recapture gets the whole session so far, an older shorter copy, or only
//! the tail of a session too long to send whole. Before any turn is added to
//! the read-only record (#1616), each message is placed: already held, or
//! new. Pure: no store, no I/O.
//!
//! Placement prefers a stable id (each Claude Code message carries a `uuid`)
//! and falls back to text only when an overlap of whole messages can be shown
//! to be unique. Text is compared message by message, never as a substring,
//! so a new turn that is only the start of an earlier one stays new. When
//! neither works the caller keeps the input whole under a flag, so no turn is
//! skipped silently and no duplicate goes unmarked.

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
    /// A run of whole messages that occurs once in the record, at its end.
    Overlap,
}

/// Why a transcript could not be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No recorded turn was found in it.
    NoOverlap,
    /// More than one place fits, or the fitting run occurs more than once in
    /// the record.
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

/// A recorded message as text placement sees it: its stored text, or `None`
/// when it cannot be compared (a message cut by truncation, or a row whose
/// messages could not be told apart). `None` equals nothing.
pub type Block<'a> = Option<&'a str>;

/// What the record holds.
pub struct Record<'a> {
    /// The recorded messages in order, as [`split_row`] gives them.
    pub blocks: &'a [Block<'a>],
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
    by_text(record.blocks, incoming.blocks)
}

/// The roles that open `blocks` (`**role:** …`), for [`split_row`].
pub fn roles(blocks: &[String]) -> HashSet<&str> {
    blocks.iter().filter_map(|b| role_of(b)).collect()
}

/// The role that opens a rendered message (`**role:** …`).
pub fn role_of(block: &str) -> Option<&str> {
    block
        .strip_prefix("**")?
        .split_once(":** ")
        .map(|(role, _)| role)
}

/// What a stored row says about itself, for [`split_row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// The row lost its start to truncation.
    pub cut: bool,
    /// How many messages the row records holding, when it records it.
    pub expected: Option<usize>,
    /// The row lists the roles of its messages.
    pub roles_listed: bool,
}

/// One stored dialog row split back into its messages.
///
/// A message starts where [`MESSAGE_SEPARATOR`] is followed by `**role:** `
/// for one of `roles`. A cut row's first piece is a fragment and compares
/// as nothing. The boundaries are trusted only when the row lists its roles
/// or its recorded count confirms the split; a split the count disagrees
/// with means a message's text held a marker. An untrusted row compares as
/// nothing, so a transcript placed against it stays unplaced.
pub fn split_row<'a>(content: &'a str, roles: &HashSet<&str>, shape: Shape) -> Vec<Block<'a>> {
    let Shape {
        cut,
        expected,
        roles_listed,
    } = shape;
    // A legacy row (no roles listed) is trusted only on a count it can be
    // checked against, which a cut row's total is not.
    if !roles_listed && (cut || expected.is_none()) {
        return vec![None];
    }
    let opens = |rest: &str| roles.iter().any(|r| starts_message(rest, r));
    let mut pieces = Vec::new();
    let mut from = 0;
    for (at, _) in content.match_indices(MESSAGE_SEPARATOR) {
        if !opens(&content[at + MESSAGE_SEPARATOR.len()..]) {
            continue;
        }
        pieces.push(&content[from..at]);
        from = at + MESSAGE_SEPARATOR.len();
    }
    pieces.push(&content[from..]);
    let whole = cut || opens(content);
    if !whole || expected.is_some_and(|n| !cut && n != pieces.len()) {
        return vec![None];
    }
    let mut blocks: Vec<Block<'a>> = pieces.into_iter().map(Some).collect();
    if cut {
        blocks[0] = None;
    }
    blocks
}

fn starts_message(text: &str, role: &str) -> bool {
    text.strip_prefix("**")
        .and_then(|t| t.strip_prefix(role))
        .is_some_and(|t| t.starts_with(":** "))
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

/// Whole-message placement in linear time: one KMP pass of the transcript
/// over the record finds whether the record holds it whole and every
/// transcript prefix the record ends with; a second pass counts the one
/// candidate prefix.
fn by_text(record: &[Block<'_>], blocks: &[String]) -> Placement {
    let pattern: Vec<&str> = blocks.iter().map(String::as_str).collect();
    let fail = failure(&pattern);
    let (whole, end) = scan(record, &pattern, &fail);
    // A copy the record already holds whole, such as an older shorter one.
    if whole > 0 {
        return Placement::Held;
    }
    // Every j where the transcript's first j messages are how the record
    // ends: the final state and its failure chain. Placed only when one such
    // j exists and its run occurs nowhere else in the record.
    let fits: Vec<usize> = std::iter::successors(Some(end), |&j| (j > 0).then(|| fail[j - 1]))
        .take_while(|&j| j > 0)
        .collect();
    match fits.as_slice() {
        [] => Placement::Unplaced(Reason::NoOverlap),
        [j] if count(record, &pattern[..*j]) == 1 => Placement::New {
            indexes: (*j..blocks.len()).collect(),
            by: Method::Overlap,
        },
        _ => Placement::Unplaced(Reason::Ambiguous),
    }
}

/// KMP failure table: `fail[i]` is the length of the longest proper prefix
/// of `pattern[..=i]` that is also its suffix.
fn failure(pattern: &[&str]) -> Vec<usize> {
    let mut fail = vec![0; pattern.len()];
    let mut k = 0;
    for i in 1..pattern.len() {
        while k > 0 && pattern[i] != pattern[k] {
            k = fail[k - 1];
        }
        if pattern[i] == pattern[k] {
            k += 1;
        }
        fail[i] = k;
    }
    fail
}

/// Run `pattern` over `record`: how many times it occurs whole, and the
/// length of the longest prefix of it the record ends with.
fn scan(record: &[Block<'_>], pattern: &[&str], fail: &[usize]) -> (usize, usize) {
    let (mut found, mut k) = (0, 0);
    for block in record {
        let Some(block) = *block else {
            k = 0;
            continue;
        };
        while k > 0 && block != pattern[k] {
            k = fail[k - 1];
        }
        if block == pattern[k] {
            k += 1;
        }
        if k == pattern.len() {
            found += 1;
            k = fail[k - 1];
        }
    }
    (found, k)
}

/// How many times the run `pattern` occurs in `record`, overlaps included.
fn count(record: &[Block<'_>], pattern: &[&str]) -> usize {
    scan(record, pattern, &failure(pattern)).0
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
        let recorded = split_row(&text, &roles(&sent), counted(record.len()));
        place(
            &Record {
                blocks: &recorded,
                ids: None,
            },
            &Incoming {
                blocks: &sent,
                ids: None,
            },
        )
    }

    /// A legacy row: no roles listed, `n` messages recorded.
    fn counted(n: usize) -> Shape {
        Shape {
            cut: false,
            expected: Some(n),
            roles_listed: false,
        }
    }

    #[test]
    fn a_legacy_row_is_trusted_only_on_a_count_it_can_be_checked_against() {
        let sent = blocks(&["x"]);
        let roles = roles(&sent);
        let row = "**user:** a\n\n**user:** b";
        let legacy = |cut: bool, expected: Option<usize>| Shape {
            cut,
            expected,
            roles_listed: false,
        };
        assert_eq!(split_row(row, &roles, legacy(false, None)), vec![None]);
        assert_eq!(split_row(row, &roles, legacy(true, Some(2))), vec![None]);
        assert_eq!(split_row(row, &roles, legacy(false, Some(2))).len(), 2);
        let listed = Shape {
            roles_listed: true,
            ..legacy(false, None)
        };
        assert_eq!(split_row(row, &roles, listed).len(), 2);
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
                    blocks: &[],
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
    fn a_new_turn_that_starts_an_earlier_one_is_not_held() {
        // "do it" is the start of the recorded "do it now", not that turn.
        let placed = by_text_of(&["a", "do it now"], &["a", "do it"]);
        assert_ne!(placed, Placement::Held);
        assert_eq!(placed, Placement::Unplaced(Reason::NoOverlap));
        // A recorded turn that is only the start of a new one is not an end.
        assert_eq!(
            by_text_of(&["a", "do it"], &["do it now", "b"]),
            Placement::Unplaced(Reason::NoOverlap)
        );
        // A turn whose text runs past a separator is still one message.
        assert_eq!(
            by_text_of(&["a", "fix it\n\nthen deploy"], &["fix it", "b"]),
            Placement::Unplaced(Reason::NoOverlap)
        );
    }

    #[test]
    fn split_row_finds_messages_and_refuses_what_it_cannot_show() {
        let sent = blocks(&["x"]);
        let roles = roles(&sent);
        let row = "**user:** a\n\nstill a\n\n**user:** b";
        assert_eq!(
            split_row(row, &roles, counted(2)),
            vec![Some("**user:** a\n\nstill a"), Some("**user:** b")]
        );
        // A count that disagrees: some text held a marker. Nothing compares.
        assert_eq!(split_row(row, &roles, counted(3)), vec![None]);
        // A row cut by truncation: its first piece is a fragment.
        assert_eq!(
            split_row(
                "tail of a\n\n**user:** b",
                &roles,
                Shape {
                    cut: true,
                    expected: None,
                    roles_listed: true
                }
            ),
            vec![None, Some("**user:** b")]
        );
    }

    #[test]
    fn an_uncomparable_block_breaks_a_run() {
        let sent = blocks(&["a", "b", "c"]);
        let recorded = [Some(sent[0].as_str()), None, Some(sent[1].as_str())];
        let placed = place(
            &Record {
                blocks: &recorded,
                ids: None,
            },
            &Incoming {
                blocks: &sent,
                ids: None,
            },
        );
        assert_eq!(placed, Placement::Unplaced(Reason::NoOverlap));
    }

    #[test]
    fn count_includes_overlapping_runs() {
        let record = [Some("y"), Some("y"), Some("y")];
        assert_eq!(count(&record, &["y", "y"]), 2);
        assert_eq!(count(&record, &["n"]), 0);
    }
}
