//! Likely duplicate entities, for a person or agent to confirm (card #65).
//!
//! An entity's id hashes its name with only case and whitespace folded, so
//! "rusty_mux", "rusty-mux" and "Rusty Mux" are three entities. This module
//! finds such pairs and reports them. It never merges: ids are the sync key,
//! and a wrong merge of two real things is worse than a visible duplicate.
//!
//! A pair is a candidate when either:
//! - **same key**: some name or alias of one has the same [`match_key`] as
//!   some name or alias of the other; or
//! - **similar**: the name keys are within a small edit distance (1 for keys
//!   of 5 to 8 characters, 2 for longer ones; never for shorter ones) *and*
//!   at least [`MIN_SHARED_MEMORIES`] live memories mention both.
//!
//! A pair is hidden once either entity lists the other's name as an alias:
//! that is how a reviewed pair is marked "seen" until a real merge exists.

use crate::db::entities::Entities;
use crate::db::{Result, Store};
use crate::entity::{normalize_entity_name, Entity};
use crate::text::edit_distance;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Live memories two similar names must share before they are reported.
pub const MIN_SHARED_MEMORIES: usize = 2;
/// Shortest name key that can match as similar ("api"/"apk" never do).
const MIN_SIMILAR_KEY_CHARS: usize = 5;
/// Name keys this long may differ by two edits instead of one.
const LONG_KEY_CHARS: usize = 9;

/// Why a pair is reported. Same-key pairs sort first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    SameKey,
    Similar,
}

/// An entity as a report names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRef {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// One likely duplicate pair, `a.id < b.id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateCandidate {
    pub a: EntityRef,
    pub b: EntityRef,
    pub reason: Reason,
    /// Live memories linked to both.
    pub shared_memories: usize,
    /// Both kinds are set and differ: maybe two real things.
    pub kind_mismatch: bool,
}

/// A likely duplicate of one entity, as its profile lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateRef {
    pub id: String,
    pub name: String,
    pub reason: Reason,
    pub shared_memories: usize,
    pub kind_mismatch: bool,
}

/// `name` folded for comparison: [`normalize_entity_name`], then `-`, `_`,
/// `.`, `/` and whitespace dropped. "Rusty-Mux", "rusty_mux" and "rusty mux"
/// all give "rustymux". Computed on read, never stored; ids do not use it.
pub fn match_key(name: &str) -> String {
    normalize_entity_name(name)
        .chars()
        .filter(|&c| !is_separator(c))
        .collect()
}

fn is_separator(c: char) -> bool {
    matches!(c, '-' | '_' | '.' | '/') || c.is_whitespace()
}

/// Every candidate pair among `entities`, same-key first, then by name.
///
/// `shared(a_id, b_id)` counts live memories linked to both; it is called
/// only for pairs whose names already match, never for every pair.
pub fn find_candidates<E>(
    entities: &[Entity],
    mut shared: impl FnMut(&str, &str) -> std::result::Result<usize, E>,
) -> std::result::Result<Vec<DuplicateCandidate>, E> {
    let mut sorted: Vec<&Entity> = entities.iter().collect();
    sorted.sort_by(|x, y| x.id.cmp(&y.id));
    let mut out = Vec::new();
    for (i, a) in sorted.iter().enumerate() {
        for b in &sorted[i + 1..] {
            let Some((reason, shared_memories)) = judge(a, b, &mut shared)? else {
                continue;
            };
            out.push(DuplicateCandidate {
                a: entity_ref(a),
                b: entity_ref(b),
                reason,
                shared_memories,
                kind_mismatch: kind_mismatch(a, b),
            });
        }
    }
    out.sort_by(|x, y| by_reason_then_name(x.reason, &x.a.name, y.reason, &y.a.name));
    Ok(out)
}

/// The candidates that pair `entity` with one of `others`, same-key first,
/// then by name.
pub fn duplicates_of<E>(
    entity: &Entity,
    others: &[Entity],
    mut shared: impl FnMut(&str, &str) -> std::result::Result<usize, E>,
) -> std::result::Result<Vec<DuplicateRef>, E> {
    let mut out = Vec::new();
    for other in others.iter().filter(|o| o.id != entity.id) {
        let Some((reason, shared_memories)) = judge(entity, other, &mut shared)? else {
            continue;
        };
        out.push(DuplicateRef {
            id: other.id.clone(),
            name: other.name.clone(),
            reason,
            shared_memories,
            kind_mismatch: kind_mismatch(entity, other),
        });
    }
    out.sort_by(|x, y| by_reason_then_name(x.reason, &x.name, y.reason, &y.name));
    Ok(out)
}

/// Every likely duplicate pair in the store.
pub fn entity_duplicates(store: &Store<'_>) -> Result<Vec<DuplicateCandidate>> {
    let repo = Entities::new(store);
    find_candidates(&repo.all()?, |a, b| repo.shared_memory_count(a, b))
}

/// The likely duplicates of `entity` in the store.
pub fn possible_duplicates(store: &Store<'_>, entity: &Entity) -> Result<Vec<DuplicateRef>> {
    let repo = Entities::new(store);
    duplicates_of(entity, &repo.all()?, |a, b| repo.shared_memory_count(a, b))
}

/// The report as plain text: one section per reason, one line per pair.
pub fn render_text(candidates: &[DuplicateCandidate]) -> String {
    if candidates.is_empty() {
        return "No likely duplicate entities.\n".to_string();
    }
    let mut out = String::new();
    for (reason, title) in [
        (Reason::SameKey, "Same name apart from case and separators"),
        (Reason::Similar, "Similar names that share memories"),
    ] {
        let rows: Vec<&DuplicateCandidate> =
            candidates.iter().filter(|c| c.reason == reason).collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("{title} ({}):\n", rows.len()));
        for c in rows {
            out.push_str(&format!(
                "  {} [{}] ~ {} [{}], {} shared{}\n",
                printable(&c.a.name),
                printable(&c.a.id),
                printable(&c.b.name),
                printable(&c.b.id),
                c.shared_memories,
                if c.kind_mismatch {
                    ", kinds differ"
                } else {
                    ""
                }
            ));
        }
    }
    out.push_str(
        "Nothing was merged. To mark a pair reviewed, add one name as an alias of the other.\n",
    );
    out
}

/// `text` with control characters escaped (`\n`, `\r`, `\u{1b}`), so a
/// stored name prints as data on one line, never as a terminal instruction.
/// Only for display; stored names and JSON values are unchanged.
fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// The reason and shared count for a pair that should be reported.
fn judge<E>(
    a: &Entity,
    b: &Entity,
    shared: &mut impl FnMut(&str, &str) -> std::result::Result<usize, E>,
) -> std::result::Result<Option<(Reason, usize)>, E> {
    let Some(reason) = name_match(a, b) else {
        return Ok(None);
    };
    let count = shared(&a.id, &b.id)?;
    if reason == Reason::Similar && count < MIN_SHARED_MEMORIES {
        return Ok(None);
    }
    Ok(Some((reason, count)))
}

/// The name evidence alone, cheapest checks first.
fn name_match(a: &Entity, b: &Entity) -> Option<Reason> {
    if lists_as_alias(a, b) || lists_as_alias(b, a) {
        return None;
    }
    let keys_b = keys(b);
    if keys(a).iter().any(|k| keys_b.contains(k)) {
        return Some(Reason::SameKey);
    }
    close(&match_key(&a.name), &match_key(&b.name)).then_some(Reason::Similar)
}

/// The non-empty match keys of an entity's name and aliases.
fn keys(entity: &Entity) -> Vec<String> {
    std::iter::once(&entity.name)
        .chain(&entity.aliases)
        .map(|n| match_key(n))
        .filter(|k| !k.is_empty())
        .collect()
}

/// `a` already lists `b`'s name as an alias: the pair was reviewed.
fn lists_as_alias(a: &Entity, b: &Entity) -> bool {
    let name = normalize_entity_name(&b.name);
    a.aliases.iter().any(|x| normalize_entity_name(x) == name)
}

/// Within the edit distance the shorter key's length allows.
fn close(a: &str, b: &str) -> bool {
    let (len_a, len_b) = (a.chars().count(), b.chars().count());
    let allowed = match len_a.min(len_b) {
        n if n < MIN_SIMILAR_KEY_CHARS => return false,
        n if n < LONG_KEY_CHARS => 1,
        _ => 2,
    };
    len_a.abs_diff(len_b) <= allowed && edit_distance(a, b) <= allowed
}

fn kind_mismatch(a: &Entity, b: &Entity) -> bool {
    match (&a.kind, &b.kind) {
        (Some(x), Some(y)) => !x.trim().eq_ignore_ascii_case(y.trim()),
        _ => false,
    }
}

fn entity_ref(e: &Entity) -> EntityRef {
    EntityRef {
        id: e.id.clone(),
        name: e.name.clone(),
        kind: e.kind.clone(),
    }
}

fn by_reason_then_name(ra: Reason, na: &str, rb: Reason, nb: &str) -> Ordering {
    ra.cmp(&rb)
        .then_with(|| normalize_entity_name(na).cmp(&normalize_entity_name(nb)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::entity_id;
    use std::collections::HashMap;
    use std::convert::Infallible;

    fn ent(name: &str, aliases: &[&str]) -> Entity {
        Entity {
            id: entity_id(name),
            name: name.into(),
            kind: None,
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn kinded(name: &str, kind: &str) -> Entity {
        Entity {
            kind: Some(kind.into()),
            ..ent(name, &[])
        }
    }

    /// Shared counts by unordered name pair; 0 when unlisted.
    fn shared_by(
        pairs: &[(&str, &str, usize)],
    ) -> impl FnMut(&str, &str) -> std::result::Result<usize, Infallible> {
        let mut map: HashMap<(String, String), usize> = HashMap::new();
        for &(x, y, n) in pairs {
            map.insert((entity_id(x), entity_id(y)), n);
            map.insert((entity_id(y), entity_id(x)), n);
        }
        move |a, b| Ok(*map.get(&(a.to_string(), b.to_string())).unwrap_or(&0))
    }

    fn found(entities: &[Entity], shared: &[(&str, &str, usize)]) -> Vec<(String, String, Reason)> {
        let mut pairs: Vec<(String, String, Reason)> = find_candidates(entities, shared_by(shared))
            .unwrap()
            .into_iter()
            .map(|c| {
                let mut names = [c.a.name, c.b.name];
                names.sort();
                let [x, y] = names;
                (x, y, c.reason)
            })
            .collect();
        pairs.sort();
        pairs
    }

    fn pair(x: &str, y: &str, r: Reason) -> (String, String, Reason) {
        (x.into(), y.into(), r)
    }

    #[test]
    fn match_key_folds_case_and_separators_only() {
        for (name, key) in [
            ("Rusty-Mux", "rustymux"),
            ("rusty_mux", "rustymux"),
            ("  Rusty   Mux ", "rustymux"),
            ("rusty.mux/v2", "rustymuxv2"),
            ("rusty-_. /mux", "rustymux"),
            ("Zürich-Ost", "zürichost"),
            ("C++", "c++"),
            ("--", ""),
        ] {
            assert_eq!(match_key(name), key, "{name:?}");
        }
    }

    #[test]
    fn three_spellings_make_one_candidate_per_pair() {
        let entities = [
            ent("rusty_mux", &[]),
            ent("rusty-mux", &[]),
            ent("Rusty Mux", &[]),
        ];
        assert_eq!(
            found(&entities, &[]),
            vec![
                pair("Rusty Mux", "rusty-mux", Reason::SameKey),
                pair("Rusty Mux", "rusty_mux", Reason::SameKey),
                pair("rusty-mux", "rusty_mux", Reason::SameKey),
            ]
        );
        for c in find_candidates(&entities, shared_by(&[])).unwrap() {
            assert!(c.a.id < c.b.id);
        }
    }

    #[test]
    fn an_alias_key_matches_the_other_name() {
        let entities = [ent("mux", &["rusty mux"]), ent("rusty_mux", &[])];
        assert_eq!(
            found(&entities, &[]),
            vec![pair("mux", "rusty_mux", Reason::SameKey)]
        );
    }

    #[test]
    fn same_key_wins_over_similar() {
        let entities = [ent("deploy-bot", &[]), ent("deploybot", &[])];
        let shared = [("deploy-bot", "deploybot", 5)];
        let got = find_candidates(&entities, shared_by(&shared)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reason, Reason::SameKey);
        assert_eq!(got[0].shared_memories, 5);
    }

    #[test]
    fn similar_names_need_two_shared_memories() {
        let entities = [ent("deploybot", &[]), ent("deploybox", &[])];
        assert!(found(&entities, &[("deploybot", "deploybox", 1)]).is_empty());
        assert_eq!(
            found(&entities, &[("deploybot", "deploybox", 2)]),
            vec![pair("deploybot", "deploybox", Reason::Similar)]
        );
    }

    #[test]
    fn distant_and_short_names_never_match_as_similar() {
        let shared = [
            ("rusty_mill", "rusty_mux", 9),
            ("ui", "uix", 9),
            ("q3", "q4", 9),
            ("api", "apk", 9),
            ("mixer", "boxer", 9),
        ];
        let entities = [
            ent("rusty_mill", &[]),
            ent("rusty_mux", &[]),
            ent("ui", &[]),
            ent("uix", &[]),
            ent("q3", &[]),
            ent("q4", &[]),
            ent("api", &[]),
            ent("apk", &[]),
            ent("mixer", &[]),
            ent("boxer", &[]),
        ];
        assert!(found(&entities, &shared).is_empty());
    }

    #[test]
    fn edit_budget_grows_with_key_length() {
        // 5-8 characters: one edit.
        assert!(close("relay", "relax"));
        assert!(!close("relay", "rebax"));
        // 9 or more: two.
        assert!(close("deploybot", "deploypot2"));
        assert!(close("orchestrator", "orkestrator"));
        assert!(!close("orchestrator", "orkestrater"));
    }

    #[test]
    fn a_short_key_still_matches_as_same_key() {
        let entities = [ent("ui", &[]), ent("UI_", &[])];
        assert_eq!(
            found(&entities, &[]),
            vec![pair("UI_", "ui", Reason::SameKey)]
        );
    }

    #[test]
    fn an_empty_key_never_matches() {
        let entities = [ent("--", &[]), ent("__", &[])];
        assert!(found(&entities, &[]).is_empty());
    }

    #[test]
    fn a_reviewed_pair_is_hidden() {
        let reviewed = [ent("rusty_mux", &["rusty-mux"]), ent("rusty-mux", &[])];
        assert!(found(&reviewed, &[]).is_empty());
        let other_way = [ent("rusty_mux", &[]), ent("rusty-mux", &["Rusty_Mux"])];
        assert!(found(&other_way, &[]).is_empty());
    }

    #[test]
    fn differing_kinds_are_flagged_not_hidden() {
        let entities = [kinded("mercury", "planet"), kinded("mer-cury", "element")];
        let got = find_candidates(&entities, shared_by(&[])).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].kind_mismatch);
        let same = [kinded("mercury", "planet"), kinded("mer-cury", "Planet")];
        assert!(!find_candidates(&same, shared_by(&[])).unwrap()[0].kind_mismatch);
    }

    #[test]
    fn duplicates_of_lists_the_others_same_key_first() {
        let me = ent("deploybot", &[]);
        let others = [
            me.clone(),
            ent("deploybox", &[]),
            ent("deploy_bot", &[]),
            ent("unrelated", &[]),
        ];
        let got = duplicates_of(&me, &others, shared_by(&[("deploybot", "deploybox", 3)])).unwrap();
        let names: Vec<(&str, Reason)> = got.iter().map(|d| (d.name.as_str(), d.reason)).collect();
        assert_eq!(
            names,
            vec![
                ("deploy_bot", Reason::SameKey),
                ("deploybox", Reason::Similar)
            ]
        );
        assert_eq!(got[1].shared_memories, 3);
    }

    #[test]
    fn shared_counts_are_asked_only_for_name_matches() {
        let entities = [
            ent("alpha", &[]),
            ent("beta", &[]),
            ent("gamma", &[]),
            ent("al-pha", &[]),
        ];
        let mut asked = Vec::new();
        find_candidates(&entities, |a: &str, b: &str| {
            asked.push((a.to_string(), b.to_string()));
            Ok::<_, Infallible>(0)
        })
        .unwrap();
        assert_eq!(asked.len(), 1);
    }

    #[test]
    fn the_text_report_groups_by_reason() {
        let entities = [
            ent("rusty_mux", &[]),
            ent("rusty-mux", &[]),
            ent("deploybot", &[]),
            ent("deploybox", &[]),
        ];
        let got = find_candidates(&entities, shared_by(&[("deploybot", "deploybox", 2)])).unwrap();
        let text = render_text(&got);
        let same = text.find("Same name").unwrap();
        let similar = text.find("Similar names").unwrap();
        assert!(same < similar, "{text}");
        assert!(text.contains("2 shared"), "{text}");
        assert_eq!(render_text(&[]), "No likely duplicate entities.\n");
    }

    #[test]
    fn the_text_report_escapes_control_characters_in_names() {
        let entities = [
            ent("rusty_mux", &[]),
            ent("rusty\nmux", &[]),
            ent("x\u{1b}[2Jforged", &["rusty mux"]),
            ent("deploy\rbot", &[]),
            ent("deploy_bot", &[]),
        ];
        let got = find_candidates(&entities, shared_by(&[])).unwrap();
        assert!(got
            .iter()
            .any(|c| c.a.name.contains('\n') || c.b.name.contains('\n')));
        let text = render_text(&got);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\r'), "{text:?}");
        assert!(text.contains("rusty\\nmux"), "{text}");
        assert!(text.contains("x\\u{1b}[2Jforged"), "{text}");
        assert!(text.contains("deploy\\rbot"), "{text}");
        // One heading, one line per pair, one footer: no line breaks from names.
        assert_eq!(text.lines().count(), 1 + got.len() + 1, "{text}");
        // The JSON report keeps the stored names as they are.
        let json = serde_json::to_string(&got).unwrap();
        assert!(json.contains("rusty\\nmux"), "{json}");
    }
}
