//! SQL-specific extraction that a query cannot express: statement splitting,
//! identifier repair and unquoting, and the facts and spans read off the tree.

use tree_sitter::Node;

use super::{collapse_ws, first_child_kind, Fact, Span};

/// Remove delimiter syntax from a SQL identifier while preserving its spelling.
/// The Sequel grammar exposes quoted identifiers with their delimiters, and its
/// T-SQL bracket form can expose only the contents as the name node.
pub(super) fn unquote_sql_identifier(name: &str) -> String {
    let Some((open, close)) = name
        .as_bytes()
        .first()
        .copied()
        .zip(name.as_bytes().last().copied())
        .filter(|(open, close)| {
            (*open == b'"' && *close == b'"')
                || (*open == b'`' && *close == b'`')
                || (*open == b'[' && *close == b']')
        })
    else {
        return name.to_string();
    };
    let inner = &name[1..name.len() - 1];
    let escaped = match (open, close) {
        (b'"', b'"') => "\"\"",
        (b'`', b'`') => "``",
        (b'[', b']') => "]]",
        _ => unreachable!(),
    };
    let delimiter = close as char;
    inner.replace(escaped, &delimiter.to_string())
}

/// The byte ranges of a SQL script's statements, in order, each ending at its
/// terminating `;` (or at end of input). Comments, quoted strings and
/// identifiers, and `$tag$ … $tag$` bodies are skipped over, so a `;` inside one
/// does not end a statement. A psql meta-command (`\echo …`, `\quit`) runs to the
/// end of its line and is not SQL, so it is left out of every range.
pub(super) fn sql_statement_ranges(src: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let push = |out: &mut Vec<(usize, usize)>, start: usize, end: usize| {
        if src[start..end].iter().any(|b| !b.is_ascii_whitespace()) {
            out.push((start, end));
        }
    };
    while i < src.len() {
        match src[i] {
            b'-' if src.get(i + 1) == Some(&b'-') => {
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < src.len() && src.get(i..i + 2) != Some(b"*/") {
                    i += 1;
                }
                i = (i + 2).min(src.len());
            }
            quote @ (b'\'' | b'"' | b'`') => {
                i += 1;
                // A doubled delimiter closes and reopens the quote, so the plain
                // scan to the next delimiter handles `''` and `""` unchanged.
                while i < src.len() && src[i] != quote {
                    i += 1;
                }
                i = (i + 1).min(src.len());
            }
            b'$' => {
                // `$tag$` / `$$` opens a body; `$1` is a parameter, not a quote.
                let mut j = i + 1;
                while j < src.len() && (src[j].is_ascii_alphanumeric() || src[j] == b'_') {
                    j += 1;
                }
                let tag_is_name = j == i + 1 || !src[i + 1].is_ascii_digit();
                if src.get(j) == Some(&b'$') && tag_is_name {
                    let delim = &src[i..=j];
                    let mut k = j + 1;
                    while k < src.len() && src.get(k..k + delim.len()) != Some(delim) {
                        k += 1;
                    }
                    i = (k + delim.len()).min(src.len());
                } else {
                    i += 1;
                }
            }
            b'\\' => {
                push(&mut out, start, i);
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
                start = i;
            }
            b';' => {
                i += 1;
                push(&mut out, start, i);
                start = i;
            }
            _ => i += 1,
        }
    }
    push(&mut out, start, src.len());
    out
}

/// Make the SQL grammar accept common escaped identifier delimiters without
/// changing source offsets. Quoted string literals and comments are skipped.
pub(super) fn repair_sql_identifier_escapes(src: &[u8]) -> Option<Vec<u8>> {
    let mut out = src.to_vec();
    let mut changed = false;
    let mut i = 0;
    while i < src.len() {
        if src.get(i..i + 2) == Some(b"--") {
            i += 2;
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if src.get(i..i + 2) == Some(b"/*") {
            i += 2;
            while i + 1 < src.len() && src.get(i..i + 2) != Some(b"*/") {
                i += 1;
            }
            i = (i + 2).min(src.len());
            continue;
        }
        if src[i] == b'\'' {
            i += 1;
            while i < src.len() {
                if src[i] == b'\'' {
                    if src.get(i + 1) == Some(&b'\'') {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if matches!(src[i], b'"' | b'`') {
            let delimiter = src[i];
            i += 1;
            while i < src.len() {
                if src[i] == delimiter {
                    if src.get(i + 1) == Some(&delimiter) {
                        out[i] = b'_';
                        out[i + 1] = b'_';
                        changed = true;
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if src[i] == b'[' && sql_column_bracket_start(src, i) {
            let mut end = i + 1;
            let mut closed = false;
            if src
                .get(end)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            {
                while end < src.len() {
                    if src[end] == b']' {
                        if src.get(end + 1) == Some(&b']') {
                            out[end] = b'_';
                            out[end + 1] = b'_';
                            changed = true;
                            end += 2;
                        } else {
                            out[i] = b'"';
                            out[end] = b'"';
                            changed = true;
                            i = end + 1;
                            closed = true;
                            break;
                        }
                    } else {
                        end += 1;
                    }
                }
                if !closed {
                    // No closing bracket was found; leave the text untouched.
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
    changed.then_some(out)
}

fn sql_column_bracket_start(src: &[u8], start: usize) -> bool {
    src[..start]
        .iter()
        .rev()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| matches!(b, b'(' | b','))
}

/// The two SQL facts a tree-sitter query cannot express, in one walk.
///
/// **Enum variants.** `CREATE TYPE mood AS ENUM ('sad', 'ok')` declares its
/// members as quoted literals, and a query captures the quotes with them —
/// `members mood` would answer `'sad'`, which no query for `sad` ever finds.
///
/// **String spans.** This grammar gives every scalar the same `literal` node, so
/// `'admin'`, `42` and `NULL` are indistinguishable by kind. Marking them all as
/// strings would push numeric and boolean literals out of `--in code`, so the
/// quote character decides: `'…'` and `$$…$$` are strings, everything else is
/// code. That keeps `search --in code email` off `'email@example.com'` without
/// hiding a threshold or a flag value.
pub(super) fn sql_extras(root: Node, src: &[u8]) -> (Vec<Fact>, Vec<(usize, usize, Span)>) {
    let mut facts = Vec::new();
    let mut spans = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
        match node.kind() {
            "literal" => {
                let text = node.utf8_text(src).unwrap_or("");
                if text.starts_with('\'') || text.starts_with('$') {
                    spans.push((node.start_byte(), node.end_byte(), Span::Str));
                }
            }
            "enum_elements" => {
                let Some(owner) = node
                    .parent()
                    .and_then(|p| first_child_kind(p, "object_reference", src))
                    .map(unqualify)
                else {
                    continue;
                };
                let mut ec = node.walk();
                for el in node.children(&mut ec) {
                    if el.kind() != "literal" {
                        continue;
                    }
                    let Ok(raw) = el.utf8_text(src) else { continue };
                    let name = raw.trim_matches('\'').to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let start = el.start_position();
                    let end = el.end_position();
                    facts.push(Fact {
                        kind: "variant".to_string(),
                        is_derived: false,
                        name,
                        start_byte: el.start_byte(),
                        end_byte: el.end_byte(),
                        start_line: start.row + 1,
                        start_col: start.column,
                        end_line: end.row + 1,
                        end_col: end.column,
                        signature: collapse_ws(raw),
                        doc: None,
                        source: "treesitter/sql".to_string(),
                        confidence: 1.0,
                        visibility: Some("public".to_string()),
                        owner: Some(owner.clone()),
                        shape: None,
                        shape_nodes: 0,
                        is_test: false,
                        decorator: None,
                        contract: None,
                    });
                }
            }
            _ => {}
        }
    }
    facts.sort_by_key(|f| f.start_byte);
    (facts, spans)
}

/// The last segment of a schema-qualified SQL name: `analytics.events` → `events`.
///
/// A definition is stored under the name it is *looked up* by. Queries write the
/// table both ways depending on `search_path`, and codeq's own reference matching
/// is textual and word-bounded — so `references events` already finds
/// `analytics.events`, while a fact stored as `analytics.events` would not answer
/// `definitions events`. The qualified form is not lost: it is what the
/// signature shows.
pub(super) fn unqualify(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}
