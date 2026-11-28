//! Old import files split into one generated capture id per conversation
//! (#528, D151, D152). Read only.
//!
//! A dialog row with no `capture_id` that came from an imported file
//! (`doc_id`, else `metadata.import_id`) would otherwise take one generated
//! id for the whole file (D150), and a chat export holds many
//! conversations. This re-reads the file's archived bytes, lists its
//! conversations the way the importer does, and gives each row the id of the
//! one conversation whose text contains it.
//!
//! Rows are matched by content, not position: the July imports carry no
//! `chunk_index`, and their mode and cut length were never recorded. A row
//! that no conversation, or more than one, contains keeps the file's id, and
//! a file with no archive, another shape, or no matching row keeps it whole.
//! Every id is a pure function of the live rows and the archive bytes, so the
//! dry run's target digest binds the apply (D77, D145).

use crate::archive::archived_bytes;
use crate::blobs::content_key;
use crate::db::{Result, Store, StoreError};
use crate::dialog_reconcile::{group_key, mint_group, MintKey, MINTED_PREFIX};
use crate::importer::{extract_messages_with_tools, filter_messages};
use crate::models::Memory;
use crate::redact::{redact, REDACTED_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Why a file kept one generated id for all its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fallback {
    /// No archive row, or its blob is gone.
    NoArchive,
    /// The archived bytes are not a JSON list of conversations.
    NotAConversationList,
    /// No row of the file was found in exactly one conversation.
    ParseMismatch,
}

impl Fallback {
    /// The report's name for this reason.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoArchive => "no_archive",
            Self::NotAConversationList => "not_a_conversation_list",
            Self::ParseMismatch => "parse_mismatch",
        }
    }
}

/// Files that kept one id, for one [`Fallback`] reason.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackCount {
    pub files: u64,
    pub rows: u64,
    pub import_ids: Vec<String>,
}

/// Files split by conversation: how many, into how many conversations, and
/// how many rows took a conversation's id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitCount {
    pub files: u64,
    pub conversations: u64,
    pub rows: u64,
}

/// Rows of split files that kept the file's id: found in no conversation,
/// or in more than one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Residual {
    pub unmatched: u64,
    pub ambiguous: u64,
}

/// Every split decided for a set of rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Splits {
    /// Memory id to the conversation's generated capture id.
    pub ids: HashMap<String, String>,
    pub split: SplitCount,
    pub fallback: BTreeMap<String, FallbackCount>,
    pub residual: Residual,
}

/// The outcome for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileOutcome {
    Fallback(Fallback),
    Split {
        /// Memory id and generated capture id, for each matched row.
        assigned: Vec<(String, String)>,
        unmatched: u64,
        ambiguous: u64,
    },
}

/// Split every imported file behind `rows`, reading archives from `store`.
///
/// # Errors
///
/// [`StoreError`] if an archive row or blob cannot be read.
pub fn resolve(store: &Store<'_>, rows: &[Memory]) -> Result<Splits> {
    let mut splits = Splits::default();
    for (import_id, members) in by_import(rows) {
        let bytes = archived_bytes(store, &import_id)
            .map_err(|e| StoreError::Engine(format!("archive of {import_id}: {e}")))?;
        let outcome = split_file(&import_id, bytes.as_deref(), &members);
        fold(&mut splits, &import_id, members.len() as u64, outcome);
    }
    Ok(splits)
}

/// Rows with no `capture_id` whose generated id would come from their file,
/// grouped by that file's import id, in a stable order.
fn by_import(rows: &[Memory]) -> BTreeMap<String, Vec<&Memory>> {
    let mut groups: BTreeMap<String, Vec<&Memory>> = BTreeMap::new();
    for m in rows.iter().filter(|m| m.capture_id.is_none()) {
        if let Some(import_id) = import_of(m) {
            groups.entry(import_id).or_default().push(m);
        }
    }
    groups
}

/// The import id D150 would group `m` by, when that is its file.
fn import_of(m: &Memory) -> Option<String> {
    match mint_group(m).0 {
        MintKey::Doc => m.doc_id.clone(),
        MintKey::Import => m
            .metadata
            .get("import_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        MintKey::Session | MintKey::Row => None,
    }
}

fn fold(splits: &mut Splits, import_id: &str, rows: u64, outcome: FileOutcome) {
    match outcome {
        FileOutcome::Fallback(why) => {
            let count = splits.fallback.entry(why.as_str().to_string()).or_default();
            count.files += 1;
            count.rows += rows;
            count.import_ids.push(import_id.to_string());
        }
        FileOutcome::Split {
            assigned,
            unmatched,
            ambiguous,
        } => {
            let conversations: BTreeSet<&String> = assigned.iter().map(|(_, id)| id).collect();
            splits.split.files += 1;
            splits.split.conversations += conversations.len() as u64;
            splits.split.rows += assigned.len() as u64;
            splits.residual.unmatched += unmatched;
            splits.residual.ambiguous += ambiguous;
            splits.ids.extend(assigned);
        }
    }
}

/// Split one file's rows by the conversations in its archived `bytes`.
pub(crate) fn split_file(import_id: &str, bytes: Option<&[u8]>, rows: &[&Memory]) -> FileOutcome {
    let Some(bytes) = bytes else {
        return FileOutcome::Fallback(Fallback::NoArchive);
    };
    let Some(conversations) = conversations(bytes) else {
        return FileOutcome::Fallback(Fallback::NotAConversationList);
    };
    let ids = conversation_ids(import_id, &conversations);
    let texts: Vec<Vec<String>> = conversations.iter().map(haystacks).collect();
    let (mut assigned, mut unmatched, mut ambiguous) = (Vec::new(), 0, 0);
    for m in rows {
        // An empty row is in every string: it places nothing.
        if m.content.is_empty() {
            unmatched += 1;
            continue;
        }
        let mut found = texts
            .iter()
            .enumerate()
            .filter(|(_, t)| t.iter().any(|h| holds(h, &m.content)));
        match (found.next(), found.next()) {
            (Some((i, _)), None) => assigned.push((m.id.clone(), ids[i].clone())),
            (None, _) => unmatched += 1,
            (Some(_), Some(_)) => ambiguous += 1,
        }
    }
    if assigned.is_empty() {
        return FileOutcome::Fallback(Fallback::ParseMismatch);
    }
    FileOutcome::Split {
        assigned,
        unmatched,
        ambiguous,
    }
}

/// The conversations of a chat export, when `bytes` are one: a JSON list
/// whose first object has `chat_messages` or `messages`, as `parse_chat`
/// tests it.
fn conversations(bytes: &[u8]) -> Option<Vec<serde_json::Value>> {
    let data: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let items = data.as_array()?;
    let first = items.first()?.as_object()?;
    if !(first.contains_key("chat_messages") || first.contains_key("messages")) {
        return None;
    }
    Some(items.clone())
}

/// One generated capture id per conversation: keyed by its own `uuid` when
/// every conversation has a distinct one, else by its position.
fn conversation_ids(import_id: &str, conversations: &[serde_json::Value]) -> Vec<String> {
    let uuids: Vec<Option<&str>> = conversations
        .iter()
        .map(|c| {
            c.get("uuid")
                .and_then(serde_json::Value::as_str)
                .filter(|u| !u.is_empty())
        })
        .collect();
    let distinct: BTreeSet<&str> = uuids.iter().flatten().copied().collect();
    let by_uuid = distinct.len() == conversations.len();
    uuids
        .iter()
        .enumerate()
        .map(|(i, uuid)| {
            let index = i.to_string();
            let key = match uuid.filter(|_| by_uuid) {
                Some(u) => group_key(&["conv", import_id, "uuid", u]),
                None => group_key(&["conv", import_id, "index", &index]),
            };
            format!("{MINTED_PREFIX}{}", &content_key(key.as_bytes())[..32])
        })
        .collect()
}

/// Each string the importer could have cut a conversation's rows from:
/// every message in each mode, and the whole exchange as one candidate.
/// Kept apart, as the importer chunks each one on its own, so a match can
/// never span two of them.
fn haystacks(conversation: &serde_json::Value) -> Vec<String> {
    let messages = extract_messages_with_tools(conversation);
    ["all_messages", "conversations", ""]
        .iter()
        .flat_map(|mode| filter_messages(&messages, mode))
        .collect()
}

/// Whether `output` holds `content`. A redaction marker stands for text the
/// raw output still has: the importer redacts each chunk after cutting, so
/// a secret across a cut leaves a marker no whole-message redaction makes.
/// The literal text around the markers must then appear in order, and the
/// source text between two literals must redact to exactly the markers it
/// replaced, in order, with nothing left over. A row that starts or ends
/// with a marker has nothing to bound that marker's source, so it cannot be
/// verified and places nothing. Matching runs only the pure redaction
/// rules, never the env switch, so `REMIND_ME_REDACT` cannot move the ids.
fn holds(output: &str, content: &str) -> bool {
    if !content.contains(REDACTED_PREFIX) {
        return output.contains(content);
    }
    let pieces = pieces(content);
    let edge_marker = |p: Option<&Piece<'_>>| p.is_some_and(|p| p.text.is_empty());
    if edge_marker(pieces.first()) || edge_marker(pieces.last()) {
        return false;
    }
    let literals: Vec<usize> = (0..pieces.len())
        .filter(|&i| !pieces[i].text.is_empty())
        .collect();
    let Some(&first) = literals.first() else {
        return false;
    };
    output
        .match_indices(pieces[first].text)
        .any(|(at, l)| follows(output, at + l.len(), &pieces, &literals[1..], first))
}

/// Whether the literals in `rest` can be placed after byte `from` of
/// `output`, each gap redacting to the kinds of the markers it replaced.
fn follows(output: &str, from: usize, pieces: &[Piece<'_>], rest: &[usize], prev: usize) -> bool {
    let Some((&next, tail)) = rest.split_first() else {
        return true;
    };
    let kinds: Vec<&str> = pieces[prev + 1..=next]
        .iter()
        .flat_map(|p| p.kinds_before.iter().copied())
        .collect();
    output[from..]
        .match_indices(pieces[next].text)
        .map(|(at, l)| (from + at, l.len()))
        .any(|(at, len)| {
            redacts_to(&output[from..at], &kinds) && follows(output, at + len, pieces, tail, next)
        })
}

/// Whether `gap` redacts to exactly the markers of `kinds`, in order.
fn redacts_to(gap: &str, kinds: &[&str]) -> bool {
    let markers: String = kinds
        .iter()
        .map(|k| format!("{REDACTED_PREFIX}{k}]"))
        .collect();
    redact(gap).content == markers
}

/// A run of literal text and the kinds of the markers just before it.
struct Piece<'a> {
    kinds_before: Vec<&'a str>,
    text: &'a str,
}

/// `content` cut at each `[REDACTED:<kind>]` marker.
fn pieces(content: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut kinds = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(REDACTED_PREFIX) {
        let tail = &rest[start + REDACTED_PREFIX.len()..];
        let Some(end) = tail.find(']') else {
            break; // An unclosed marker is literal text.
        };
        out.push(Piece {
            kinds_before: std::mem::take(&mut kinds),
            text: &rest[..start],
        });
        kinds.push(&tail[..end]);
        rest = &tail[end + 1..];
    }
    out.push(Piece {
        kinds_before: kinds,
        text: rest,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, content: &str) -> Memory {
        let new = crate::db::memories::NewMemory {
            metadata: serde_json::json!({ "import_id": "imp1" }),
            ..crate::db::memories::NewMemory::new(id, content, "2026-07-01T00:00:00Z")
        };
        crate::db::engine::memories::MemoryRow::from_new(&new).to_memory()
    }

    fn export(conversations: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&conversations).unwrap()
    }

    fn two_chats(uuids: bool) -> Vec<u8> {
        let chat = |uuid: &str, text: &str| {
            let mut c = serde_json::json!({ "chat_messages": [
                { "sender": "human", "text": format!("ask {text}") },
                { "sender": "assistant", "text": format!("answer {text}") },
            ]});
            if uuids {
                c["uuid"] = serde_json::json!(uuid);
            }
            c
        };
        export(serde_json::json!([chat("u1", "alpha"), chat("u2", "beta")]))
    }

    fn assigned(outcome: &FileOutcome) -> Vec<(String, String)> {
        match outcome {
            FileOutcome::Split { assigned, .. } => assigned.clone(),
            FileOutcome::Fallback(why) => panic!("fell back: {why:?}"),
        }
    }

    #[test]
    fn rows_take_the_id_of_the_conversation_that_holds_them() {
        let rows = [
            row("m1", "ask alpha"),
            row("m2", "answer alpha"),
            row("m3", "ask beta"),
        ];
        let refs: Vec<&Memory> = rows.iter().collect();

        let got = assigned(&split_file("imp1", Some(&two_chats(true)), &refs));

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, got[1].1, "one conversation, one id");
        assert_ne!(got[0].1, got[2].1, "two conversations, two ids");
        assert!(got.iter().all(|(_, id)| id.starts_with(MINTED_PREFIX)));
    }

    #[test]
    fn uuids_key_the_id_and_their_absence_falls_back_to_position() {
        let rows = [row("m1", "ask beta")];
        let refs: Vec<&Memory> = rows.iter().collect();

        let by_uuid = assigned(&split_file("imp1", Some(&two_chats(true)), &refs));
        let by_index = assigned(&split_file("imp1", Some(&two_chats(false)), &refs));
        let uuid_key = group_key(&["conv", "imp1", "uuid", "u2"]);
        let index_key = group_key(&["conv", "imp1", "index", "1"]);

        assert_eq!(
            by_uuid[0].1,
            format!("{MINTED_PREFIX}{}", &content_key(uuid_key.as_bytes())[..32])
        );
        assert_eq!(
            by_index[0].1,
            format!(
                "{MINTED_PREFIX}{}",
                &content_key(index_key.as_bytes())[..32]
            )
        );
    }

    #[test]
    fn a_row_in_no_conversation_or_in_two_keeps_the_file_id() {
        let rows = [
            row("m1", "ask alpha"),
            row("m2", "never said"),
            row("m3", "answer"),
        ];
        let refs: Vec<&Memory> = rows.iter().collect();

        let outcome = split_file("imp1", Some(&two_chats(true)), &refs);

        let FileOutcome::Split {
            assigned,
            unmatched,
            ambiguous,
        } = outcome
        else {
            panic!("expected a split");
        };
        assert_eq!((assigned.len(), unmatched, ambiguous), (1, 1, 1));
    }

    #[test]
    fn files_that_cannot_be_split_say_why() {
        let rows = [row("m1", "ask alpha")];
        let refs: Vec<&Memory> = rows.iter().collect();
        let other =
            |r: &[&Memory]| split_file("imp1", Some(br#"[{"role":"user","content":"hi"}]"#), r);

        assert_eq!(
            split_file("imp1", None, &refs),
            FileOutcome::Fallback(Fallback::NoArchive)
        );
        assert_eq!(
            other(&refs),
            FileOutcome::Fallback(Fallback::NotAConversationList)
        );
        let stray = [row("m9", "not in the file")];
        let stray: Vec<&Memory> = stray.iter().collect();
        assert_eq!(
            split_file("imp1", Some(&two_chats(true)), &stray),
            FileOutcome::Fallback(Fallback::ParseMismatch)
        );
    }

    #[test]
    fn a_row_redacted_at_import_matches_even_with_redaction_off_now() {
        let secret = "card 4111 1111 1111 1111 please";
        let bytes = export(serde_json::json!([
            { "uuid": "u1", "chat_messages": [{ "sender": "human", "text": secret }] },
            { "uuid": "u2", "chat_messages": [{ "sender": "human", "text": "other" }] },
        ]));
        let stored = redact(secret).content;
        assert_ne!(stored, secret, "the fixture must hold a secret");
        let rows = [row("m1", &stored)];
        let refs: Vec<&Memory> = rows.iter().collect();

        crate::test_env::set_var("REMIND_ME_REDACT", "0");
        let off = split_file("imp1", Some(&bytes), &refs);
        crate::test_env::remove_var("REMIND_ME_REDACT");
        let on = split_file("imp1", Some(&bytes), &refs);

        assert_eq!(assigned(&off).len(), 1);
        assert_eq!(off, on, "the environment does not change the ids");
    }

    #[test]
    fn a_match_across_two_importer_outputs_or_an_empty_row_places_nothing() {
        // "alpha\n\nbeta" only exists across two messages' outputs; the
        // other conversation holds neither.
        let bytes = export(serde_json::json!([
            { "uuid": "u1", "chat_messages": [
                { "sender": "human", "text": "alpha" },
                { "sender": "assistant", "text": "beta" },
            ]},
            { "uuid": "u2", "chat_messages": [{ "sender": "human", "text": "gamma" }] },
        ]));
        let rows = [row("m1", "] alpha\n\n[assistant"), row("m2", "")];
        let refs: Vec<&Memory> = rows.iter().collect();

        let outcome = split_file("imp1", Some(&bytes), &refs);

        assert_eq!(outcome, FileOutcome::Fallback(Fallback::ParseMismatch));
    }

    #[test]
    fn a_secret_cut_across_chunks_still_places_its_row() {
        // The importer cut inside the key, then redacted each chunk: the
        // second chunk keeps the key's tail and a redacted card, which no
        // whole-message redaction produces.
        let key_tail = "aB3".repeat(40);
        let message = format!("sk-{key_tail} card 4111 1111 1111 1111 please");
        let chunk = redact(&format!("{key_tail} card 4111 1111 1111 1111 please")).content;
        assert!(chunk.contains(REDACTED_PREFIX));
        assert!(!redact(&message).content.contains(&chunk));
        let bytes = export(serde_json::json!([
            { "uuid": "u1", "chat_messages": [{ "sender": "human", "text": message }] },
            { "uuid": "u2", "chat_messages": [{ "sender": "human", "text": "other" }] },
        ]));
        let rows = [row("m1", &chunk)];
        let refs: Vec<&Memory> = rows.iter().collect();

        assert_eq!(assigned(&split_file("imp1", Some(&bytes), &refs)).len(), 1);
    }

    #[test]
    fn literals_around_markers_must_appear_in_order() {
        let card = "card [REDACTED:card_number] please";
        assert!(holds("card 4111 1111 1111 1111 please", card));
        assert!(
            !holds("card is not needed please", card),
            "the gap must redact"
        );
        assert!(!holds("please card 4111 1111 1111 1111", card));
        assert!(
            !holds("card old 4111 1111 1111 1111 please", card),
            "surplus gap text"
        );
        let two = "a [REDACTED:card_number] b [REDACTED:card_number] c";
        assert!(holds("a 4111 1111 1111 1111 b 5500 0000 0000 0004 c", two));
        assert!(
            !holds("a 4111 1111 1111 1111 b c", two),
            "one card, two markers"
        );
        assert!(!holds("anything", "[REDACTED:card_number]"));
        assert!(
            !holds("x please", "[REDACTED:card_number] please"),
            "unverified edge"
        );
        assert!(
            !holds("card x", "card [REDACTED:card_number]"),
            "unverified edge"
        );
        assert!(holds("a [REDACTED:open", "a [REDACTED:open"));
    }
}
