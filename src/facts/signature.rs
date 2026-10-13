//! The two cheap-understanding fields of a fact: its `signature` (the
//! declaration with the body stripped) and its `doc` comment.

use tree_sitter::Node;

/// Cap signatures/docs so a pathological declaration can't blow the budget.
pub(super) const MAX_SIG: usize = 200;
pub(super) const MAX_DOC: usize = 240;

/// Put back a hint of what a folded multi-line value *is*.
///
/// A constant's signature stops where its value begins, which reads well when a
/// wrapper survives the cut (`const Card = memo(`) and badly when nothing does:
/// a 52-entry dict — the substance of its file — rendered as a bare `alias =`,
/// which looks like the renderer gave up rather than like a deliberate fold.
/// Appending the bracket the value opens with costs three characters and says
/// which kind of thing was folded.
pub(super) fn with_value_shape(sig: String, kind: &str, val: Option<Node>) -> String {
    if kind != "constant" || !sig.trim_end().ends_with('=') {
        return sig;
    }
    let Some(v) = val else { return sig };
    let shape = match v.kind() {
        "dictionary" | "object" | "set" | "block_mapping" => "{…}",
        "list" | "array" | "list_display" => "[…]",
        "tuple" | "arguments" | "parenthesized_expression" => "(…)",
        _ => return sig,
    };
    format!("{} {shape}", sig.trim_end())
}

/// For a value that wraps something — `memo(…)`, `styled.div\`…\`` — the node
/// where the wrapped part starts, so a signature can be cut there and keep the
/// wrapper. `None` when the value is not a wrapper (a bare object or array
/// literal), in which case the caller cuts at the value itself.
pub(super) fn wrapper_head(val: Node) -> Option<Node> {
    match val.kind() {
        "call_expression" => val.child_by_field_name("arguments"),
        "tagged_template_expression" => val.child_by_field_name("quasi"),
        _ => None,
    }
}

/// The declaration text with any body stripped, collapsed to one line and
/// capped. When there is no body capture (structs, consts, imports, …) the
/// whole node text is used, still capped.
pub(super) fn signature_of(src: &[u8], def: Node, body: Option<Node>) -> String {
    if let Some(sig) = arrow_signature(src, def) {
        return cap(&sig, MAX_SIG);
    }
    let end = match body {
        Some(b) => b.start_byte(),
        None => def.end_byte(),
    };
    let raw = &src[def.start_byte()..end.min(src.len())];
    cap(&collapse_ws(&String::from_utf8_lossy(raw)), MAX_SIG)
}

/// The callable signature of an arrow function bound to a name, or `None` when
/// `def` is not one.
///
/// The definition node for `const withCDN = (path: string) => {…}` is the
/// declarator, so taking the source up to the body yields `withCDN = (path:
/// string) =>` — an assignment fragment ending in a dangling arrow, where every
/// other definition in the same outline reads as a signature. Arrow functions
/// are how most JS/TS code declares a function, so this was the common case
/// rendering worse than the rare one. Composed from the parameter and
/// return-type fields instead, which also puts the parentheses back on the
/// unparenthesized single-parameter form (`const f = x => x`).
fn arrow_signature(src: &[u8], def: Node) -> Option<String> {
    let (name, value) = match def.kind() {
        "variable_declarator" => ("name", "value"),
        "assignment_expression" => ("left", "right"),
        _ => return None,
    };
    let arrow = def.child_by_field_name(value)?;
    if arrow.kind() != "arrow_function" {
        return None;
    }
    let text = |n: Node| collapse_ws(&String::from_utf8_lossy(&src[n.start_byte()..n.end_byte()]));
    let params = match arrow.child_by_field_name("parameters") {
        Some(p) => text(p),
        // A lone parameter needs no parentheses in source; a signature does.
        None => format!("({})", text(arrow.child_by_field_name("parameter")?)),
    };
    // `async` is a bare token with no field, and it changes what the caller gets
    // back, so it belongs in the signature.
    let asynchronous = arrow
        .children(&mut arrow.walk())
        .any(|c| c.kind() == "async");
    let generics = arrow
        .child_by_field_name("type_parameters")
        .map(text)
        .unwrap_or_default();
    let returns = arrow
        .child_by_field_name("return_type")
        .map(text)
        .unwrap_or_default();
    Some(format!(
        "{}{}{generics}{params}{returns}",
        if asynchronous { "async " } else { "" },
        text(def.child_by_field_name(name)?),
    ))
}

pub(super) fn doc_of(src: &[u8], lang: &str, def: Node, body: Option<Node>) -> Option<String> {
    let raw = match lang {
        "python" => python_doc(src, body?),
        "rust" => rust_doc(src, def),
        _ => None,
    }?;
    let s = cap(&collapse_ws(&raw), MAX_DOC);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Python docstring: the first statement in a body if it is a bare string.
fn python_doc(src: &[u8], body: Node) -> Option<String> {
    let first = body.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let expr = first.named_child(0)?;
    if expr.kind() != "string" {
        return None;
    }
    let text = expr.utf8_text(src).ok()?;
    Some(text.trim_matches(|c| c == '"' || c == '\'').to_string())
}

/// Rust doc comments: contiguous `///` lines immediately preceding the item,
/// skipping attribute lines between them and the item.
fn rust_doc(src: &[u8], def: Node) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut sib = def.prev_sibling();
    while let Some(node) = sib {
        match node.kind() {
            "line_comment" => {
                let t = node.utf8_text(src).ok()?;
                if let Some(rest) = t.strip_prefix("///") {
                    lines.push(rest.trim().to_string());
                    sib = node.prev_sibling();
                    continue;
                }
                break;
            }
            "attribute_item" => {
                sib = node.prev_sibling();
                continue;
            }
            _ => break,
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join(" "))
}

pub(super) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}
