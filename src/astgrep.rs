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
        "dart" => tree_sitter_dart::LANGUAGE.into(),
        "zig" => tree_sitter_zig::LANGUAGE.into(),
        "elixir" => tree_sitter_elixir::LANGUAGE.into(),
        "solidity" => tree_sitter_solidity::LANGUAGE.into(),
        "sql" => tree_sitter_sequel::LANGUAGE.into(),
        // Markup only: a `<script>` body is one opaque token to these grammars,
        // so JS/TS patterns can't reach inside a Svelte or Vue component.
        "svelte" => tree_sitter_svelte_ng::LANGUAGE.into(),
        "vue" => tree_sitter_vue_next::LANGUAGE.into(),
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
        let transparent = n.id() == root.id() || k == "expression_statement" || k == "ERROR";
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
    pat.utf8_text(psrc)
        .ok()
        .and_then(|t| t.strip_prefix(SENTINEL))
}

/// If `pat` is a variadic metavariable — a `$$$` leaf, or one wrapped in a
/// single-child node (a `$$$` in statement position parses as an
/// `expression_statement` around the sentinel) — return its name (empty =
/// anonymous).
fn variadic_name<'a>(pat: Node, psrc: &'a [u8]) -> Option<&'a str> {
    let mut n = pat;
    loop {
        match n.named_child_count() {
            0 => {
                return n
                    .utf8_text(psrc)
                    .ok()
                    .and_then(|t| t.strip_prefix(VARIADIC))
            }
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
    // `variadic_name` deliberately sees through single-child wrappers, because a
    // `$$$` in statement position parses as an `expression_statement` around the
    // sentinel. That over-reaches for a *list* node holding one `$$$`: the
    // `parameters` of `fn $N($$$)` looks like a variadic itself, when really the
    // absorption belongs one level down, inside it. With two such lists in one
    // pattern — `fn $N($$$) { $$$ }`, the single most natural thing to write —
    // the >1-variadic guard below then rejected the whole pattern and reported
    // no matches at all. So when the arities already line up, match positionally
    // and let the recursion absorb inside each child; fall back to absorbing at
    // this level only when they don't.
    if pkids.len() == ckids.len() {
        let mut trial = binds.clone();
        if pkids
            .iter()
            .zip(ckids)
            .all(|(p, c)| node_matches(*p, *c, psrc, csrc, &mut trial))
        {
            *binds = trial;
            return true;
        }
    }

    let vpos: Vec<usize> = pkids
        .iter()
        .enumerate()
        .filter(|(_, n)| variadic_name(**n, psrc).is_some())
        .map(|(i, _)| i)
        .collect();

    if vpos.is_empty() {
        return false; // arities differ and there is nothing to absorb the gap
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

/// The literal text a match must contain: every pattern leaf that is not a
/// metavariable. A leaf matches by exact text, so a file missing any of these
/// cannot match, whatever its structure.
fn required_literals<'a>(pat: Node, psrc: &'a [u8]) -> Vec<&'a [u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut stack = vec![pat];
    while let Some(node) = stack.pop() {
        if node.named_child_count() == 0 {
            let text = &psrc[node.start_byte()..node.end_byte()];
            let metavar =
                text.starts_with(SENTINEL.as_bytes()) || text.starts_with(VARIADIC.as_bytes());
            if !metavar && !text.is_empty() && !out.contains(&text) {
                out.push(text);
            }
            continue;
        }
        let mut cur = node.walk();
        stack.extend(node.named_children(&mut cur));
    }
    out
}

/// Parse one file and append every node matching the pattern.
fn match_file(
    parser: &mut Parser,
    pat_node: Node,
    psrc: &[u8],
    path: &str,
    body: &str,
    out: &mut Vec<Match>,
) {
    let Some(tree) = parser.parse(body, None) else {
        return;
    };
    let pat_kind = pat_node.kind_id();
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
                    path: path.to_string(),
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
    // `significant` descends through ERROR nodes, because a bare expression is not
    // a valid top-level item in some grammars and only survives via error
    // recovery. The cost is that a genuinely malformed pattern also parses — the
    // parser discards what it can't use, and the descent lands on whichever
    // fragment survived, error-free. `if { }` quietly became "an empty block" and
    // `garbage $$$` became a lone identifier: both then reported zero matches and
    // exit 0, which reads as "no such code exists" rather than "your pattern is
    // wrong". Requiring the chosen node to span the whole pattern separates the
    // two, since recovery is exactly what drops part of the input.
    let covered =
        std::str::from_utf8(&psrc[pat_node.start_byte()..pat_node.end_byte()]).unwrap_or_default();
    if covered.trim() != substituted.trim() {
        return Err(anyhow!(
            "pattern did not parse as valid {lang_name}; write it as a complete construct"
        ));
    }
    if metavar_name(pat_node, psrc).is_some() || variadic_name(pat_node, psrc).is_some() {
        return Err(anyhow!(
            "pattern is a bare metavariable — it would match everything"
        ));
    }
    // A file can only match if it holds every literal the pattern spells out,
    // so most files are ruled out by a substring test and never parsed. What
    // survives is parsed on every core: re-parsing is the whole cost here, and
    // one file's parse owes nothing to another's.
    let literals = required_literals(pat_node, psrc);
    let finders: Vec<memchr::memmem::Finder> = literals
        .iter()
        .map(|lit| memchr::memmem::Finder::new(lit))
        .collect();
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8);
    // Bounded, so bodies are still streamed rather than all held at once.
    let (work_tx, work_rx) = std::sync::mpsc::sync_channel::<(String, String)>(workers * 2);
    let work_rx = std::sync::Mutex::new(work_rx);
    let mut out = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    // A parser and a pattern tree per worker: neither is shared.
                    let mut out = Vec::new();
                    let mut parser = Parser::new();
                    if parser.set_language(&lang).is_err() {
                        return out;
                    }
                    let Some(pat_tree) = parser.parse(&substituted, None) else {
                        return out;
                    };
                    let pat_node = significant(pat_tree.root_node());
                    loop {
                        let next = work_rx.lock().unwrap().recv();
                        let Ok((path, body)) = next else {
                            return out;
                        };
                        match_file(&mut parser, pat_node, psrc, &path, &body, &mut out);
                    }
                })
            })
            .collect();
        for (path, body) in files {
            let possible = finders.iter().all(|f| f.find(body.as_bytes()).is_some());
            if possible && work_tx.send((path, body)).is_err() {
                break;
            }
        }
        drop(work_tx);
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("astgrep worker panicked"))
            .collect::<Vec<Match>>()
    });
    out.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.start_col.cmp(&b.start_col))
    });
    Ok(out)
}

/// One node in an `--explain` tree walk: what a pattern would have to name.
pub struct TreeNode {
    /// Nesting relative to the node the walk started at.
    pub depth: usize,
    /// The grammar's node kind — the thing a pattern is written against.
    pub kind: String,
    /// The field this node fills in its parent (`function`, `name`, `body`, …),
    /// where the grammar names one. Fields are the stable part of a grammar, so
    /// this is worth more than the kind when the kinds differ across languages.
    pub field: Option<String>,
    pub line: usize,
    pub col: usize,
    /// The source this node covers, collapsed to one line and capped.
    pub text: String,
}

/// The parse tree covering one line of a file: the innermost node that contains
/// the whole line, and everything under it that touches the line.
///
/// This exists because `astgrep` requires exact node kinds and offered no way to
/// learn them — the failure mode of a wrong guess is an empty result that reads
/// exactly like "no matches", which is the worst kind of wrong answer for a tool
/// whose whole claim is determinism.
///
/// Anonymous nodes (punctuation, operators) are omitted: a query matches named
/// nodes, and `$$$` covers the rest, so listing every `(` and `,` would bury the
/// kinds a pattern is actually written from.
pub fn explain(language: &str, src: &str, line: usize) -> Result<Vec<TreeNode>> {
    let lang =
        ts_language(language).ok_or_else(|| anyhow!("no grammar for language '{language}'"))?;
    let mut parser = Parser::new();
    parser.set_language(&lang)?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow!("failed to parse as {language}"))?;

    // The line's byte range, so "contains the line" is a byte comparison.
    let mut start = 0usize;
    let mut found = None;
    for (i, l) in src.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            found = Some((start, start + l.trim_end_matches(['\n', '\r']).len()));
            break;
        }
        start += l.len();
    }
    // A line past the end of the file has no tree. Returning the root instead —
    // which is what a byte range clamped to `src.len()` would have selected — is
    // the failure this whole command exists to prevent: a plausible-looking
    // answer to a question that has none.
    let Some((lo, hi)) = found else {
        return Ok(Vec::new());
    };
    let hi = hi.max(lo);

    // Descend to the innermost node that still contains the whole line, so the
    // walk starts at the statement rather than at the file.
    let mut root = tree.root_node();
    loop {
        let mut cursor = root.walk();
        let next = root
            .children(&mut cursor)
            .find(|c| c.start_byte() <= lo && c.end_byte() >= hi);
        match next {
            Some(c) => root = c,
            None => break,
        }
    }

    let mut out = Vec::new();
    let mut stack = vec![(root, 0usize, None::<String>)];
    while let Some((node, depth, field)) = stack.pop() {
        // Only what the line actually covers: a `create_table` spanning fifty
        // lines should not dump all fifty.
        if node.end_byte() < lo || node.start_byte() > hi {
            continue;
        }
        if node.is_named() {
            let text = node.utf8_text(src.as_bytes()).unwrap_or("");
            let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let text = if collapsed.chars().count() > 60 {
                let cut: String = collapsed.chars().take(60).collect();
                format!("{cut}…")
            } else {
                collapsed
            };
            out.push(TreeNode {
                depth,
                kind: node.kind().to_string(),
                field,
                line: node.start_position().row + 1,
                col: node.start_position().column,
                text,
            });
        }
        // Collect children with the field name the grammar gives each, in one
        // cursor pass — `field_name()` is a property of the cursor's position,
        // so it cannot be recovered from a `Node` afterwards.
        let mut cursor = node.walk();
        let mut children: Vec<(Node, Option<String>)> = Vec::new();
        if cursor.goto_first_child() {
            loop {
                children.push((cursor.node(), cursor.field_name().map(|s| s.to_string())));
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        let d = if node.is_named() { depth + 1 } else { depth };
        // Pushed in reverse so the stack yields them left to right.
        for (child, field) in children.into_iter().rev() {
            stack.push((child, d, field));
        }
    }
    Ok(out)
}
