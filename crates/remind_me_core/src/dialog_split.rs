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
/// in more than one, or in a search that hit [`MAX_SPANS`] before every
/// other conversation was ruled out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Residual {
    pub unmatched: u64,
    pub ambiguous: u64,
    pub unresolved: u64,
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
        residual: Residual,
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
        let outcome = split_file(&import_id, bytes.as_deref(), &members, MAX_SPANS);
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
        FileOutcome::Split { assigned, residual } => {
            let conversations: BTreeSet<&String> = assigned.iter().map(|(_, id)| id).collect();
            splits.split.files += 1;
            splits.split.conversations += conversations.len() as u64;
            splits.split.rows += assigned.len() as u64;
            splits.residual.unmatched += residual.unmatched;
            splits.residual.ambiguous += residual.ambiguous;
            splits.residual.unresolved += residual.unresolved;
            splits.ids.extend(assigned);
        }
    }
}

/// Split one file's rows by the conversations in its archived `bytes`,
/// trying at most `cap` candidate spans per importer output.
pub(crate) fn split_file(
    import_id: &str,
    bytes: Option<&[u8]>,
    rows: &[&Memory],
    cap: usize,
) -> FileOutcome {
    let Some(bytes) = bytes else {
        return FileOutcome::Fallback(Fallback::NoArchive);
    };
    let Some(conversations) = conversations(bytes) else {
        return FileOutcome::Fallback(Fallback::NotAConversationList);
    };
    let ids = conversation_ids(import_id, &conversations);
    let texts: Vec<Vec<String>> = conversations.iter().map(haystacks).collect();
    let (mut assigned, mut residual) = (Vec::new(), Residual::default());
    for m in rows {
        match place(&texts, &m.content, cap) {
            Placement::One(i) => assigned.push((m.id.clone(), ids[i].clone())),
            Placement::None => residual.unmatched += 1,
            Placement::Many => residual.ambiguous += 1,
            Placement::Unresolved => residual.unresolved += 1,
        }
    }
    if assigned.is_empty() {
        return FileOutcome::Fallback(Fallback::ParseMismatch);
    }
    FileOutcome::Split { assigned, residual }
}

/// Where one row belongs among a file's conversations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// Exactly one conversation holds it and every other is ruled out.
    One(usize),
    None,
    Many,
    /// A search hit its cap, so some conversation is neither found nor
    /// ruled out.
    Unresolved,
}

fn place(texts: &[Vec<String>], content: &str, cap: usize) -> Placement {
    // An empty row is in every string: it places nothing.
    if content.is_empty() {
        return Placement::None;
    }
    let (mut found, mut unknown) = (Vec::new(), false);
    for (i, outputs) in texts.iter().enumerate() {
        match conversation_holds(outputs, content, cap) {
            Found::Yes => found.push(i),
            Found::Unknown => unknown = true,
            Found::No => {}
        }
    }
    match (found.as_slice(), unknown) {
        ([_, _, ..], _) => Placement::Many,
        (_, true) => Placement::Unresolved,
        ([i], false) => Placement::One(*i),
        ([], false) => Placement::None,
    }
}

/// A match in any output settles the conversation; otherwise an exhausted
/// search leaves it unknown.
fn conversation_holds(outputs: &[String], content: &str, cap: usize) -> Found {
    let mut verdict = Found::No;
    for output in outputs {
        match holds(output, content, cap) {
            Found::Yes => return Found::Yes,
            Found::Unknown => verdict = Found::Unknown,
            Found::No => {}
        }
    }
    verdict
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

/// Most candidate spans one output is tried with for one redacted row, so a
/// short, common literal cannot make the dry run quadratic. An output with
/// more candidates is neither a match nor ruled out, and its row stays
/// residual (`unresolved`), the same on every run.
const MAX_SPANS: usize = 256;

/// Whether one search of an importer output found the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Yes,
    No,
    /// `cap` candidates were tried without a match and more were left.
    Unknown,
}

/// Whether `output` holds `content`. A row with redaction markers is how
/// the importer stored a chunk: it cut a span of the output, then redacted
/// that span on its own. So it matches when some span of `output` that
/// starts with the row's first literal and ends with its last redacts
/// (pure rules, never the env switch) to exactly the row. The span keeps
/// each secret's own context (`password:` before it, the URL around it),
/// and a secret across a cut redacts as the chunk did. A row that starts
/// or ends with a marker has nothing to bound that marker's source, so it
/// cannot be verified and places nothing.
fn holds(output: &str, content: &str, cap: usize) -> Found {
    let markers = markers(content);
    let (Some(&(first_at, _)), Some(&(_, last_end))) = (markers.first(), markers.last()) else {
        return found(output.contains(content));
    };
    let (first, last) = (&content[..first_at], &content[last_end..]);
    if first.is_empty() || last.is_empty() {
        return Found::No;
    }
    let spans = occurrences(output, first).flat_map(|at| {
        let after = at + first.len();
        occurrences(&output[after..], last).map(move |end| (at, after + end + last.len()))
    });
    for (tried, (at, end)) in spans.enumerate() {
        if tried == cap {
            return Found::Unknown;
        }
        if redact(&output[at..end]).content == content {
            return Found::Yes;
        }
    }
    Found::No
}

/// Every start of `needle` in `hay`, overlapping ones included: a chunk can
/// begin inside a run such as `:::`, which `match_indices` would skip.
fn occurrences<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let mut from = 0;
    std::iter::from_fn(move || {
        let at = from + hay.get(from..)?.find(needle)?;
        let step = hay[at..].chars().next().map_or(1, char::len_utf8);
        from = at + step;
        Some(at)
    })
}

fn found(yes: bool) -> Found {
    if yes {
        Found::Yes
    } else {
        Found::No
    }
}

/// The byte ranges of each closed `[REDACTED:<kind>]` marker in `content`.
/// An unclosed one is literal text.
fn markers(content: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = content[from..].find(REDACTED_PREFIX) {
        let start = from + at;
        let Some(close) = content[start..].find(']') else {
            break;
        };
        let end = start + close + 1;
        out.push((start, end));
        from = end;
    }
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

        let got = assigned(&split_file(
            "imp1",
            Some(&two_chats(true)),
            &refs,
            MAX_SPANS,
        ));

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, got[1].1, "one conversation, one id");
        assert_ne!(got[0].1, got[2].1, "two conversations, two ids");
        assert!(got.iter().all(|(_, id)| id.starts_with(MINTED_PREFIX)));
    }

    #[test]
    fn uuids_key_the_id_and_their_absence_falls_back_to_position() {
        let rows = [row("m1", "ask beta")];
        let refs: Vec<&Memory> = rows.iter().collect();

        let by_uuid = assigned(&split_file(
            "imp1",
            Some(&two_chats(true)),
            &refs,
            MAX_SPANS,
        ));
        let by_index = assigned(&split_file(
            "imp1",
            Some(&two_chats(false)),
            &refs,
            MAX_SPANS,
        ));
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

        let outcome = split_file("imp1", Some(&two_chats(true)), &refs, MAX_SPANS);

        let FileOutcome::Split { assigned, residual } = outcome else {
            panic!("expected a split");
        };
        assert_eq!(assigned.len(), 1);
        assert_eq!(
            residual,
            Residual {
                unmatched: 1,
                ambiguous: 1,
                unresolved: 0
            }
        );
    }

    #[test]
    fn files_that_cannot_be_split_say_why() {
        let rows = [row("m1", "ask alpha")];
        let refs: Vec<&Memory> = rows.iter().collect();
        let other = |r: &[&Memory]| {
            split_file(
                "imp1",
                Some(br#"[{"role":"user","content":"hi"}]"#),
                r,
                MAX_SPANS,
            )
        };

        assert_eq!(
            split_file("imp1", None, &refs, MAX_SPANS),
            FileOutcome::Fallback(Fallback::NoArchive)
        );
        assert_eq!(
            other(&refs),
            FileOutcome::Fallback(Fallback::NotAConversationList)
        );
        let stray = [row("m9", "not in the file")];
        let stray: Vec<&Memory> = stray.iter().collect();
        assert_eq!(
            split_file("imp1", Some(&two_chats(true)), &stray, MAX_SPANS),
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
        let off = split_file("imp1", Some(&bytes), &refs, MAX_SPANS);
        crate::test_env::remove_var("REMIND_ME_REDACT");
        let on = split_file("imp1", Some(&bytes), &refs, MAX_SPANS);

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

        let outcome = split_file("imp1", Some(&bytes), &refs, MAX_SPANS);

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

        assert_eq!(
            assigned(&split_file("imp1", Some(&bytes), &refs, MAX_SPANS)).len(),
            1
        );
    }

    fn yes(output: &str, content: &str) -> bool {
        holds(output, content, MAX_SPANS) == Found::Yes
    }

    fn no(output: &str, content: &str) -> bool {
        holds(output, content, MAX_SPANS) == Found::No
    }

    #[test]
    fn literals_around_markers_must_appear_in_order() {
        let card = "card [REDACTED:card_number] please";
        assert!(yes("card 4111 1111 1111 1111 please", card));
        assert!(no("card is not needed please", card), "the gap must redact");
        assert!(no("please card 4111 1111 1111 1111", card));
        assert!(
            no("card old 4111 1111 1111 1111 please", card),
            "surplus gap text"
        );
        let two = "a [REDACTED:card_number] b [REDACTED:card_number] c";
        assert!(yes("a 4111 1111 1111 1111 b 5500 0000 0000 0004 c", two));
        assert!(
            no("a 4111 1111 1111 1111 b c", two),
            "one card, two markers"
        );
        assert!(no("anything", "[REDACTED:card_number]"));
        assert!(
            no("x please", "[REDACTED:card_number] please"),
            "unverified edge"
        );
        assert!(
            no("card x", "card [REDACTED:card_number]"),
            "unverified edge"
        );
        assert!(yes("a [REDACTED:open", "a [REDACTED:open"));
        // The secret's context lives in the literal before it.
        let stored = redact("password: hunter2hunter2 please").content;
        assert!(stored.contains(REDACTED_PREFIX), "{stored}");
        assert!(yes("said password: hunter2hunter2 please, ok", &stored));
    }

    #[test]
    fn overlapping_literals_are_each_a_candidate_start_and_end() {
        let card = "4111 1111 1111 1111";
        let row = redact(&format!("::{card} please")).content;
        assert!(row.starts_with("::["), "{row}");
        assert!(yes(&format!(":::{card} please"), &row), "start inside :::");
        let tail = redact(&format!("see {card} ..")).content;
        assert!(tail.ends_with("] .."), "{tail}");
        assert!(yes(&format!("see {card} ..."), &tail), "end inside ...");
        assert_eq!(occurrences("aaa", "aa").collect::<Vec<_>>(), [0, 1]);
        assert_eq!(occurrences("éé", "é").collect::<Vec<_>>(), [0, 2]);
    }

    #[test]
    fn a_search_that_tests_every_candidate_at_the_cap_is_decided() {
        let card = "card [REDACTED:card_number] please";
        // Three candidate spans, none of them the row.
        let three = "card x please card y please";
        assert_eq!(holds(three, card, 3), Found::No, "exactly at the cap");
        assert_eq!(holds(three, card, 2), Found::Unknown, "one skipped");
        // The third candidate is the row.
        let last = "card x please card 4111 1111 1111 1111 please";
        assert_eq!(holds(last, card, 3), Found::Yes);
        assert_eq!(holds(last, card, 2), Found::Unknown);
    }

    #[test]
    fn an_early_match_does_not_rule_out_an_exhausted_conversation() {
        let bytes = export(serde_json::json!([
            { "uuid": "u1", "chat_messages": [
                { "sender": "human", "text": "card 4111 1111 1111 1111 please" },
            ]},
            { "uuid": "u2", "chat_messages": [
                { "sender": "human", "text": "card x please card y please" },
            ]},
        ]));
        // m2 is plain text in u2 only, so the file still splits.
        let rows = [
            row("m1", "card [REDACTED:card_number] please"),
            row("m2", "card x please"),
        ];
        let refs: Vec<&Memory> = rows.iter().collect();

        let FileOutcome::Split { assigned, residual } = split_file("imp1", Some(&bytes), &refs, 1)
        else {
            panic!("expected a split");
        };
        let full = self::assigned(&split_file("imp1", Some(&bytes), &refs, MAX_SPANS));

        assert_eq!(
            assigned
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["m2"]
        );
        assert_eq!(residual.unresolved, 1, "m1 keeps the file id");
        assert_eq!(full.len(), 2, "u2 is ruled out once fully searched");
        assert_ne!(full[0].1, full[1].1);
    }

    #[test]
    fn a_match_settles_its_own_conversation_despite_an_exhausted_output() {
        // u1's first message and the whole exchange exhaust the cap; its
        // second message holds the row on the first candidate.
        let bytes = export(serde_json::json!([
            { "uuid": "u1", "chat_messages": [
                { "sender": "human", "text": "card x please card y please" },
                { "sender": "assistant", "text": "card 4111 1111 1111 1111 please" },
            ]},
            { "uuid": "u2", "chat_messages": [{ "sender": "human", "text": "other" }] },
        ]));
        let rows = [row("m1", "card [REDACTED:card_number] please")];
        let refs: Vec<&Memory> = rows.iter().collect();

        assert_eq!(
            assigned(&split_file("imp1", Some(&bytes), &refs, 1)).len(),
            1
        );
    }
}
