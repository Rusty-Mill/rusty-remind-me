//! Turning arbitrary user text into a valid FTS5 `MATCH` expression.

/// Convert a raw query into a safe FTS5 `MATCH` expression.
///
/// FTS5 treats a lot of ordinary punctuation as operator syntax — `?`, `,`,
/// `'`, `$`, `.`, `-` and more — so a natural-language question passed through
/// unchanged is frequently not a valid expression at all, and SQLite answers
/// with a syntax error rather than no results. Bound parameters do not help:
/// the value still has to parse as a MATCH expression once bound.
///
/// Each word token is extracted and wrapped in double quotes, which also stops
/// a token like `or`, `and` or `near` being read as an operator. Tokens are
/// joined with `OR` so any of them can match; BM25 still ranks by term
/// importance, so common words do not dominate the results.
///
/// Returns an empty string when there is nothing searchable. Callers must treat
/// that as "no results" rather than passing it to `MATCH`, which would itself
/// be a syntax error.
pub fn sanitize_fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        // FTS5 escapes a literal double quote by doubling it.
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();

    tokens.join(" OR ")
}

/// Pull an `entity:NAME` (or `entity:"Full Name"`) token out of a query.
///
/// Matches the reference's structured-query syntax (`FT-04`): a token
/// prefixed `entity:` narrows results to memories linked to that entity,
/// leaving the rest of the query as ordinary free text. This is a shared,
/// reusable extraction, not folded into `GET /api/memories/search`
/// specifically, because the MCP `remind_me_search` tool's own
/// `subject:`/`predicate:`/`entity:` structured-query support is a separate,
/// still-open gap — see the parity tracker — and whichever fixes that should
/// reuse this rather than write a second parser.
///
/// Returns `(entity_query, remaining_free_text)`. Only the first `entity:`
/// token is honoured, matching the reference (a regex `.search`, not a global
/// substitution of the token pattern — though every occurrence of the literal
/// token is still stripped from the remaining text).
pub fn extract_entity_token(query: &str) -> (Option<String>, String) {
    let mut found: Option<(usize, usize, String)> = None;

    for (start, _) in query.char_indices() {
        if !query[start..].starts_with("entity:") {
            continue;
        }
        let after = start + "entity:".len();
        let rest = &query[after..];
        if let Some(quoted) = rest.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => {
                    found = Some((start, after + end + 2, quoted[..end].to_string()));
                    break;
                }
                // An unterminated quote names no entity. Keep scanning rather
                // than falling through to the unquoted case, which would
                // otherwise treat the bare `"` as part of an unquoted token.
                None => continue,
            }
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if word_end == 0 {
            continue; // "entity:" with nothing after it names no entity.
        }
        found = Some((start, after + word_end, rest[..word_end].to_string()));
        break;
    }

    let Some((start, end, value)) = found else {
        return (None, query.to_string());
    };

    let remaining: String = format!("{}{}", &query[..start], &query[end..])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (Some(value), remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_become_quoted_alternatives() {
        assert_eq!(sanitize_fts_query("hello world"), "\"hello\" OR \"world\"");
    }

    #[test]
    fn punctuation_that_fts5_would_choke_on_is_stripped() {
        // A natural-language question: every one of ? ' , would otherwise be
        // read as operator syntax.
        assert_eq!(
            sanitize_fts_query("what's the plan, exactly?"),
            "\"what\" OR \"s\" OR \"the\" OR \"plan\" OR \"exactly\""
        );
    }

    #[test]
    fn operator_words_are_quoted_rather_than_parsed() {
        assert_eq!(
            sanitize_fts_query("this AND that NEAR other"),
            "\"this\" OR \"AND\" OR \"that\" OR \"NEAR\" OR \"other\""
        );
    }

    #[test]
    fn underscores_survive_as_part_of_a_token() {
        // Identifiers like `memory_tags` should stay whole.
        assert_eq!(sanitize_fts_query("memory_tags"), "\"memory_tags\"");
    }

    #[test]
    fn a_query_with_no_word_characters_yields_nothing() {
        assert_eq!(sanitize_fts_query("?!  ..."), "");
        assert_eq!(sanitize_fts_query(""), "");
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(sanitize_fts_query("say \"hi\""), "\"say\" OR \"hi\"");
    }

    #[test]
    fn an_unquoted_entity_token_is_extracted() {
        assert_eq!(
            extract_entity_token("quokkas entity:Rottnest habitat"),
            (Some("Rottnest".to_string()), "quokkas habitat".to_string())
        );
    }

    #[test]
    fn a_quoted_entity_token_keeps_its_spaces() {
        assert_eq!(
            extract_entity_token("entity:\"Bailey Robertson\" notes"),
            (Some("Bailey Robertson".to_string()), "notes".to_string())
        );
    }

    #[test]
    fn a_query_with_no_entity_token_is_returned_whole() {
        assert_eq!(
            extract_entity_token("what is a quokka"),
            (None, "what is a quokka".to_string())
        );
    }

    #[test]
    fn an_entity_only_query_leaves_no_free_text() {
        assert_eq!(
            extract_entity_token("entity:Rottnest"),
            (Some("Rottnest".to_string()), String::new())
        );
    }

    #[test]
    fn only_the_first_entity_token_is_honoured() {
        assert_eq!(
            extract_entity_token("entity:Rottnest entity:Tasmania"),
            (Some("Rottnest".to_string()), "entity:Tasmania".to_string())
        );
    }

    #[test]
    fn a_trailing_entity_prefix_with_nothing_after_it_names_no_entity() {
        assert_eq!(
            extract_entity_token("quokkas entity:"),
            (None, "quokkas entity:".to_string())
        );
    }

    #[test]
    fn an_unterminated_quote_does_not_swallow_the_rest_of_the_query() {
        assert_eq!(
            extract_entity_token("entity:\"Rottnest habitat notes"),
            (None, "entity:\"Rottnest habitat notes".to_string())
        );
    }
}
