//! Single-file components (Svelte, Vue): the `<script>` blocks are re-parsed as
//! JavaScript or TypeScript in the file's own coordinates.

use tree_sitter::Node;

use super::{first_child_kind, parse_health, Extracted, Fact, LangSpec};

/// A single-file component (Svelte `.svelte`, Vue `.vue`). The code an agent
/// cares about lives in `<script>`, which these grammars leave as one opaque
/// `raw_text` token — so each block is re-parsed with the JavaScript or
/// TypeScript extractor (per its `lang` attribute) over a copy of the file with
/// everything *outside* the block blanked out. Blanking rather than slicing keeps
/// every byte offset, line and column in the file's own coordinates, so the
/// nested facts need no remapping and `read`/`at` address them like any other
/// definition.
///
/// `markup` contributes whatever the template itself defines (Svelte's
/// `{#snippet}`; Vue's template defines nothing) plus comment/attribute spans;
/// `<style>` contents stay unclassified (no CSS grammar), and the file itself
/// yields a `component` fact.
pub(super) fn extract_component(
    markup: &mut LangSpec,
    javascript: &mut LangSpec,
    typescript: &mut LangSpec,
    path: &str,
    src: &[u8],
) -> Extracted {
    let tree = match markup.parser.parse(src, None) {
        Some(t) => t,
        None => return ((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()),
    };
    let root = tree.root_node();
    let (mut facts, mut spans, mut relations) = markup.harvest(root, src);
    let mut health = parse_health(root);
    let mut calls = Vec::new();

    for (start, end, is_ts) in script_regions(root, src) {
        let blanked = blank_outside(src, start, end);
        let ((f, s, r), nested_health, nested_calls) = if is_ts {
            typescript.extract(&blanked)
        } else {
            javascript.extract(&blanked)
        };
        if let Some(nested) = nested_health {
            health.errors += nested.errors;
            health.recovered |= nested.recovered;
        }
        facts.extend(f);
        spans.extend(s);
        relations.extend(r);
        calls.extend(nested_calls);
    }
    facts.extend(component_fact(path, src));
    spans.sort_by_key(|(s, _, _)| *s);
    ((facts, spans, relations), Some(health), calls)
}

/// The byte range of each `<script>` block's *contents*, plus whether the block
/// declares TypeScript. Walked from the tree rather than queried so the one
/// parse in `extract_svelte` serves both passes.
pub(crate) fn script_regions(root: Node, src: &[u8]) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        if node.kind() == "script_element" {
            if let Some(raw) = node.children(&mut c).find(|n| n.kind() == "raw_text") {
                out.push((raw.start_byte(), raw.end_byte(), script_is_ts(node, src)));
            }
            continue; // contents are one token — nothing further to walk
        }
        for child in node.children(&mut c) {
            stack.push(child);
        }
    }
    out.sort_by_key(|(s, _, _)| *s);
    out
}

/// Whether a `<script>` element is TypeScript, i.e. carries `lang="ts"` (or
/// `lang="typescript"`). Anything else — including no `lang` at all — is
/// JavaScript, which is Svelte's own default.
fn script_is_ts(script: Node, src: &[u8]) -> bool {
    let mut c = script.walk();
    let tag = match script.children(&mut c).find(|n| n.kind() == "start_tag") {
        Some(t) => t,
        None => return false,
    };
    let mut tc = tag.walk();
    for attr in tag.children(&mut tc).filter(|n| n.kind() == "attribute") {
        if first_child_kind(attr, "attribute_name", src) != Some("lang") {
            continue;
        }
        let mut ac = attr.walk();
        let value = attr
            .children(&mut ac)
            .find(|n| matches!(n.kind(), "quoted_attribute_value" | "attribute_value"))
            .and_then(|n| n.utf8_text(src).ok())
            .map(|t| t.trim_matches(|c| c == '"' || c == '\''));
        return matches!(value, Some("ts") | Some("typescript"));
    }
    false
}

/// A copy of `src` with every byte outside `[start, end)` replaced by a space,
/// newlines excepted. Parsing this instead of the slice keeps the embedded
/// region at its original byte offsets, rows and columns, so facts extracted
/// from it are already in file coordinates.
pub(crate) fn blank_outside(src: &[u8], start: usize, end: usize) -> Vec<u8> {
    let mut buf = src.to_vec();
    for (i, b) in buf.iter_mut().enumerate() {
        if (i < start || i >= end) && *b != b'\n' {
            *b = b' ';
        }
    }
    buf
}

/// A `.svelte` file *is* a component, named by its file stem and used as
/// `<Button …/>` after `import Button from './Button.svelte'` — there is no
/// declaration in the source to capture, so the file itself is the definition.
/// SvelteKit route files (`+page.svelte`, `+layout.svelte`) are loaded by
/// convention and never referenced by name, so they get none: a fact there could
/// only ever look unused.
fn component_fact(path: &str, src: &[u8]) -> Option<Fact> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    if stem.is_empty() || stem.starts_with('+') {
        return None;
    }
    let newlines = src.iter().filter(|b| **b == b'\n').count();
    let last_line_start = src.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    Some(Fact {
        kind: "component".to_string(),
        is_derived: false,
        name: stem.to_string(),
        start_byte: 0,
        end_byte: src.len(),
        start_line: 1,
        start_col: 0,
        end_line: newlines + 1,
        end_col: src.len() - last_line_start,
        signature: format!("<{stem} />"),
        doc: None,
        // Not a captured node: derived from the file's own identity, and marked
        // as such so the provenance stays honest.
        source: "svelte/component".to_string(),
        confidence: 1.0,
        visibility: Some("public".to_string()), // importable by any other file
        owner: None,
        shape: None,
        shape_nodes: 0,
        is_test: false,
        decorator: None,
        contract: None,
    })
}
