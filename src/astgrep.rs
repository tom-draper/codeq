//! Structural (AST) search — ast-grep-style pattern matching.
//!
//! A *pattern* is a snippet of code with metavariables, parsed with the target
//! language's own grammar, so matching is structural (on the syntax tree), not
//! textual: `$X.unwrap()` finds every `.unwrap()` call whatever the receiver,
//! spacing, or line breaks. It is deterministic and exact — a node matches iff
//! its kind and named-child structure match.
//!
//! - `$NAME` / `$_` — one node (any). A repeated `$NAME` must bind identically,
//!   so `$X == $X` matches only self-comparisons.
//! - `$$$NAME` / `$$$` — a run of zero-or-more sibling nodes in a list position,
//!   so `foo($$$)` matches any argument count and `fn $N($$$)` any parameters.
//!
//! Limits: matching is exact about structure, so a pattern must include optional
//! syntax that the code has (e.g. `fn $N($$$) { $$$ }` won't match a fn *with* a
//! return type); and it can't see inside macro invocations, whose bodies the
//! grammar leaves as opaque token trees.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser};

/// A `$VAR` is rewritten to this prefix + the name so the pattern parses as
/// ordinary code; long enough to never collide with a real identifier.
const SENTINEL: &str = "Cq9Metavar_";
/// A `$$$VAR` (variadic) is rewritten to this prefix — it matches a run of zero
/// or more sibling nodes in a list position (arguments, parameters, a block, …).
const VARIADIC: &str = "Cq9Variadic_";

pub struct Match {
    pub path: String,
    pub start_byte: usize,
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub text: String,
    /// Metavariable bindings (name -> matched source), sorted by name.
    pub binds: Vec<(String, String)>,
}

/// Map a codeq language name to its tree-sitter grammar.
pub fn ts_language(name: &str) -> Option<tree_sitter::Language> {
    Some(match name {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        "lua" => tree_sitter_lua::LANGUAGE.into(),
        "scala" => tree_sitter_scala::LANGUAGE.into(),
        "kotlin" => tree_sitter_kotlin_ng::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        _ => return None,
    })
}

/// Rewrite `$NAME` / `$_` / `$$$NAME` to sentinel identifiers so the pattern
/// parses as ordinary code.
fn substitute_metavars(pattern: &str) -> Result<String> {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let mut dollars = 1;
            while chars.peek() == Some(&'$') {
                chars.next();
                dollars += 1;
            }
            let mut name = String::new();
            while let Some(&n) = chars.peek() {
                if n == '_' || n.is_ascii_alphanumeric() {
                    name.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            if dollars >= 3 {
                out.push_str(VARIADIC); // $$$ or $$$NAME
                out.push_str(&name);
                continue;
            } else if dollars == 1 && !name.is_empty() {
                out.push_str(SENTINEL); // $NAME or $_
                out.push_str(&name);
                continue;
            }
            for _ in 0..dollars {
                out.push('$');
            }
            out.push_str(&name);
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

/// Descend from the parse root to the meaningful pattern node, past *transparent*
/// wrappers only — the file root, an `expression_statement`, and error-recovery
/// nodes (a bare expression pattern doesn't parse at top level in some grammars).
/// A node that carries its own keyword (`return_expression`, `if_expression`, …)
/// is NOT stripped, so `return Ok($X)` stays anchored to `return`.
fn significant(root: Node) -> Node {
    let mut n = root;
    loop {
        let mut cur = n.walk();
        let named: Vec<Node> = n.named_children(&mut cur).collect();
        if named.len() != 1 {
            return n;
        }
        let k = n.kind();
        let transparent =
            n.id() == root.id() || k == "expression_statement" || k == "ERROR";
        if transparent {
            n = named[0];
        } else {
            return n;
        }
    }
}

/// If `pat` is a single-node metavariable leaf, return its name (`_` = anonymous).
fn metavar_name<'a>(pat: Node, psrc: &'a [u8]) -> Option<&'a str> {
    if pat.named_child_count() != 0 {
        return None; // metavars are leaf identifiers, never an ancestor
    }
    pat.utf8_text(psrc).ok().and_then(|t| t.strip_prefix(SENTINEL))
}

/// If `pat` is a variadic metavariable — a `$$$` leaf, or one wrapped in a
/// single-child node (a `$$$` in statement position parses as an
/// `expression_statement` around the sentinel) — return its name (empty =
/// anonymous).
fn variadic_name<'a>(pat: Node, psrc: &'a [u8]) -> Option<&'a str> {
    let mut n = pat;
    loop {
        match n.named_child_count() {
            0 => return n.utf8_text(psrc).ok().and_then(|t| t.strip_prefix(VARIADIC)),
            1 => n = n.named_child(0).unwrap(),
            _ => return None,
        }
    }
}

fn bind(binds: &mut HashMap<String, String>, name: &str, val: String) -> bool {
    match binds.get(name) {
        Some(prev) => prev == &val, // repeated metavar must bind identically
        None => {
            binds.insert(name.to_string(), val);
            true
        }
    }
}

fn node_matches(
    pat: Node,
    code: Node,
    psrc: &[u8],
    csrc: &[u8],
    binds: &mut HashMap<String, String>,
) -> bool {
    if let Some(name) = metavar_name(pat, psrc) {
        if name == "_" {
            return true; // anonymous metavar: any single node
        }
        let ctext = match code.utf8_text(csrc) {
            Ok(t) => t.to_string(),
            Err(_) => return false,
        };
        return bind(binds, name, ctext);
    }
    if pat.kind_id() != code.kind_id() {
        return false;
    }
    let mut pc = pat.walk();
    let mut cc = code.walk();
    let pkids: Vec<Node> = pat.named_children(&mut pc).collect();
    let ckids: Vec<Node> = code.named_children(&mut cc).collect();
    if pkids.is_empty() {
        // Leaf (identifier, literal, operator): match text exactly.
        return pat.utf8_text(psrc).ok() == code.utf8_text(csrc).ok();
    }
    match_children(&pkids, &ckids, psrc, csrc, binds)
}

/// Match a pattern child list against a code child list, where the pattern may
/// contain one variadic (`$$$`) that absorbs a run of zero or more code nodes.
fn match_children(
    pkids: &[Node],
    ckids: &[Node],
    psrc: &[u8],
    csrc: &[u8],
    binds: &mut HashMap<String, String>,
) -> bool {
    let vpos: Vec<usize> = pkids
        .iter()
        .enumerate()
        .filter(|(_, n)| variadic_name(**n, psrc).is_some())
        .map(|(i, _)| i)
        .collect();

    if vpos.is_empty() {
        return pkids.len() == ckids.len()
            && pkids
                .iter()
                .zip(ckids)
                .all(|(p, c)| node_matches(*p, *c, psrc, csrc, binds));
    }
    // A single variadic per list is supported (the common case); more would need
    // backtracking, so treat it as no match rather than guess.
    if vpos.len() > 1 {
        return false;
    }
    let vi = vpos[0];
    let before = &pkids[..vi];
    let after = &pkids[vi + 1..];
    if ckids.len() < before.len() + after.len() {
        return false;
    }
    for (p, c) in before.iter().zip(&ckids[..before.len()]) {
        if !node_matches(*p, *c, psrc, csrc, binds) {
            return false;
        }
    }
    let a_start = ckids.len() - after.len();
    for (p, c) in after.iter().zip(&ckids[a_start..]) {
        if !node_matches(*p, *c, psrc, csrc, binds) {
            return false;
        }
    }
    // Bind the absorbed run (named variadic only), for consistency across repeats.
    if let Some(name) = variadic_name(pkids[vi], psrc) {
        if !name.is_empty() {
            let joined = ckids[before.len()..a_start]
                .iter()
                .map(|n| n.utf8_text(csrc).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\u{0}");
            return bind(binds, name, joined);
        }
    }
    true
}

/// Compile `pattern` for `lang_name` and return every structural match across
/// `files` (each `(path, source)`), in deterministic path+position order.
pub fn search<I>(lang_name: &str, pattern: &str, files: I) -> Result<Vec<Match>>
where
    I: IntoIterator<Item = (String, String)>,
{
    let lang = ts_language(lang_name).ok_or_else(|| anyhow!("unknown language: {lang_name}"))?;
    let substituted = substitute_metavars(pattern)?;

    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| anyhow!("set language: {e}"))?;
    let pat_tree = parser
        .parse(&substituted, None)
        .ok_or_else(|| anyhow!("failed to parse pattern"))?;
    let psrc = substituted.as_bytes();
    let pat_node = significant(pat_tree.root_node());
    if pat_node.is_error() || pat_node.has_error() {
        return Err(anyhow!(
            "pattern did not parse as valid {lang_name}; write it as a complete construct"
        ));
    }
    if metavar_name(pat_node, psrc).is_some() || variadic_name(pat_node, psrc).is_some() {
        return Err(anyhow!("pattern is a bare metavariable — it would match everything"));
    }
    let pat_kind = pat_node.kind_id();

    let mut out = Vec::new();
    for (path, body) in files {
        let tree = match parser.parse(&body, None) {
            Some(t) => t,
            None => continue,
        };
        let csrc = body.as_bytes();
        // DFS; only same-kind nodes can match; a matched node's subtree is skipped
        // so nested duplicates of the same pattern aren't reported.
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind_id() == pat_kind {
                let mut binds = HashMap::new();
                if node_matches(pat_node, node, psrc, csrc, &mut binds) {
                    let sp = node.start_position();
                    let ep = node.end_position();
                    let mut b: Vec<(String, String)> = binds.into_iter().collect();
                    b.sort();
                    out.push(Match {
                        path: path.clone(),
                        start_byte: node.start_byte(),
                        start_line: sp.row + 1,
                        start_col: sp.column,
                        end_line: ep.row + 1,
                        text: node.utf8_text(csrc).unwrap_or("").to_string(),
                        binds: b,
                    });
                    continue;
                }
            }
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                stack.push(child);
            }
        }
    }
    out.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.start_col.cmp(&b.start_col))
    });
    Ok(out)
}
