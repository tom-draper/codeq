//! Structural call sites: which identifiers are actually being *called*, and on
//! what receiver, taken from the parse tree instead of the bytes around them.
//!
//! `references`/`callers` narrow textually first (the trigram index plus a
//! word-boundary regex), which is fast and never misses. The `--calls` and
//! `--on` filters then ask a question text cannot answer:
//!
//! - `fn parse_range(` is followed by `(` exactly as `parse_range(x)` is;
//! - `db\n    .query()` is the same call as `db.query()`;
//! - in `a.b.query()` the receiver is the expression `a.b`, not `b`.
//!
//! So when either filter is used, the file is parsed once and every call site in
//! it is collected: a map from the byte offset of the called identifier to its
//! receiver, if any. Checking a candidate is then a lookup.
//!
//! This runs only for those two opt-in filters, so ordinary queries pay nothing
//! and the index does not grow. A language with no grammar yields no map and the
//! caller falls back to the byte-level tests; in practice both filters already
//! imply code-only, which excludes files codeq cannot parse, so the fallback is
//! defensive rather than load-bearing.
//!
//! Grammars differ, so this reads tree-sitter *field names* (`function`,
//! `object`, `receiver`, `name`, …) rather than node-kind spellings wherever it
//! can. Fields are the part of a grammar that is deliberately stable and
//! consistent across languages; node kinds are not.

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

/// The receiver of a call, as written: `None` for a plain `f()`, `Some("db")`
/// for `db.query()`, `Some("a.b")` for `a.b.query()`.
pub type Receiver = Option<String>;

/// Byte offset of each called identifier in a file, mapped to its receiver.
pub type CallMap = HashMap<usize, Receiver>;

/// One syntax-derived invocation, suitable for storing in the repository index.
/// `name` is the final identifier as written; `receiver` is retained separately
/// so consumers can distinguish `db.query()` from a free `query()` call.
pub struct CallSite {
    pub start_byte: usize,
    pub name: String,
    pub receiver: Receiver,
}

/// Fields naming the called function on a call node, most specific first.
const CALLEE_FIELDS: &[&str] = &["function", "method", "name", "constructor"];
/// Fields naming the receiver, on either the call node or a member-access node.
const RECEIVER_FIELDS: &[&str] =
    &["object", "receiver", "operand", "target", "value", "expression"];
/// Fields naming the member being selected on a member-access node.
const MEMBER_FIELDS: &[&str] = &[
    "field", "property", "attribute", "name", "selector", "suffix",
];

/// Every call site in `src`, or `None` when the language has no grammar — or has
/// one that cannot see the code.
///
/// A single-file component (`.svelte`, `.vue`) *does* have a grammar, but that
/// grammar is a markup grammar: the `<script>` body comes back as one opaque
/// `raw_text` node, so parsing finds no call expressions inside it at all. That
/// is worse than having no grammar, because callers take `Some(map)` as
/// authoritative and an empty map reads as "nothing is called here" — which
/// silently emptied `callees` for every component. Returning `None` sends them to
/// the byte-level scan, which can see into the script.
pub fn calls_in(language: &str, src: &str) -> Option<CallMap> {
    Some(
        call_sites_in(language, src)?
            .into_iter()
            .map(|site| (site.start_byte, site.receiver))
            .collect(),
    )
}

/// Every syntax-derived invocation in `src`, in source order.
pub fn call_sites_in(language: &str, src: &str) -> Option<Vec<CallSite>> {
    if !supports_syntax_calls(language) {
        return None;
    }
    let lang = crate::astgrep::ts_language(language)?;
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(src, None)?;

    let mut sites = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if is_call(node) {
            if let Some((name_node, receiver)) = callee_of(node, src) {
                sites.push(CallSite {
                    start_byte: name_node.start_byte(),
                    name: text_of(name_node, src),
                    receiver,
                });
            }
        }
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
    }
    sites.sort_by_key(|site| site.start_byte);
    Some(sites)
}

/// Whether a language's parser exposes its ordinary call expressions. Svelte
/// and Vue deliberately return false: their component grammar leaves script
/// bodies opaque, so treating its empty call list as authoritative would lose
/// real calls that the textual fallback can still find.
pub fn supports_syntax_calls(language: &str) -> bool {
    !matches!(language, "svelte" | "vue") && crate::astgrep::ts_language(language).is_some()
}

/// Whether a node invokes something.
///
/// Kind-name matching is the fallback of last resort, but call nodes are the one
/// place grammars agree on vocabulary: `call_expression`, `call`,
/// `method_invocation`, `invocation_expression`, `function_call_expression`,
/// `object_creation_expression`. Requiring an argument list as well keeps a
/// stray match from counting.
fn is_call(node: Node) -> bool {
    let k = node.kind();
    if !(k.contains("call") || k.contains("invocation")) {
        return false;
    }
    // Swift's `call_suffix` is *part* of a call, not a call itself, and its kind
    // contains "call" too.
    if k.ends_with("_suffix") {
        return false;
    }
    // An argument list confirms it, but Ruby's `db.query` (and Kotlin/Scala
    // paren-less calls) have none, so a field naming what is being invoked
    // counts as well.
    if first_field(node, CALLEE_FIELDS).is_some() || first_field(node, MEMBER_FIELDS).is_some() {
        return true;
    }
    let mut c = node.walk();
    let found = node.children(&mut c).any(|ch| {
        let ck = ch.kind();
        ck.contains("argument") || ck.contains("call_suffix") || ck == "("
    });
    found
}

/// The identifier a call names, plus the receiver it is called on.
///
/// Two shapes, both handled by fields:
/// - the call node carries `name`/`method` and `object`/`receiver` itself
///   (Java `method_invocation`, Ruby `call`, PHP `member_call_expression`);
/// - the call node carries `function`, which is either a bare identifier or a
///   member access to unwrap (Rust, Python, JS/TS, Go, C, C++, C#).
fn callee_of<'a>(call: Node<'a>, src: &str) -> Option<(Node<'a>, Receiver)> {
    // Shape 1: everything on the call node.
    if let Some(name) = first_field(call, MEMBER_FIELDS) {
        if is_identifier(name) {
            let recv = first_field(call, RECEIVER_FIELDS).map(|n| text_of(n, src));
            return Some((name, recv));
        }
    }

    // Shape 2: a `function` (or similar) to unwrap.
    let callee = match first_field(call, CALLEE_FIELDS) {
        Some(n) => n,
        None => {
            // No usable field: the callee is the first named child that is not
            // the argument list.
            let mut c = call.walk();
            let first = call
                .named_children(&mut c)
                .find(|ch| !ch.kind().contains("argument"));
            first?
        }
    };

    if is_identifier(callee) {
        // Ruby puts the method in `method` and the receiver on the call node
        // itself, so a bare callee identifier can still be qualified.
        let recv = first_field(call, RECEIVER_FIELDS).map(|n| text_of(n, src));
        return Some((callee, recv));
    }

    // Some grammars wrap the callee in a transparent node (Solidity's
    // `expression`), so step through single-child wrappers to the real one.
    let callee = unwrap_wrapper(callee);

    // Unwrapping can land on the identifier itself, when the grammar wraps a bare
    // callee in a reference node — SQL's `count(*)` is
    // `invocation(object_reference(identifier))`. The member-access path below
    // looks *inside* the node it is given, so a leaf there finds nothing and the
    // call is silently not a call.
    if is_identifier(callee) {
        let recv = first_field(call, RECEIVER_FIELDS).map(|n| text_of(n, src));
        return Some((callee, recv));
    }

    // A member access: the last identifier is the method, the object is the
    // receiver as written (`a.b` in `a.b.query()`).
    let name = first_field(callee, MEMBER_FIELDS)
        .map(name_leaf)
        .or_else(|| last_identifier_child(callee))?;
    if !is_identifier(name) {
        return None;
    }
    let recv = match first_field(callee, RECEIVER_FIELDS) {
        Some(n) => Some(n),
        None => {
            let mut c = callee.walk();
            let other = callee
                .named_children(&mut c)
                .find(|ch| ch.id() != name.id());
            other
        }
    }
    .map(|n| text_of(n, src));
    Some((name, recv))
}

fn first_field<'a>(node: Node<'a>, fields: &[&str]) -> Option<Node<'a>> {
    fields.iter().find_map(|f| node.child_by_field_name(f))
}

/// Step through nodes that exist only to wrap another (Solidity's `expression`),
/// which carry no fields of their own and one named child.
fn unwrap_wrapper(mut node: Node) -> Node {
    while !is_identifier(node)
        && node.named_child_count() == 1
        && first_field(node, MEMBER_FIELDS).is_none()
        && first_field(node, RECEIVER_FIELDS).is_none()
    {
        match node.named_child(0) {
            Some(child) => node = child,
            None => break,
        }
    }
    node
}

/// The identifier inside a member node that is not itself a leaf: Swift's
/// `navigation_suffix` holds the method name one level down.
fn name_leaf(node: Node) -> Node {
    if is_identifier(node) {
        return node;
    }
    first_field(node, MEMBER_FIELDS)
        .filter(|n| is_identifier(*n))
        .or_else(|| last_identifier_child(node))
        .unwrap_or(node)
}

fn last_identifier_child(node: Node) -> Option<Node> {
    let mut c = node.walk();
    let last = node.named_children(&mut c).filter(is_identifier_ref).last();
    last
}

/// A leaf naming something. Grammars spell it `identifier`,
/// `field_identifier`, `property_identifier`, `simple_identifier`, `name`, …
fn is_identifier(node: Node) -> bool {
    let k = node.kind();
    (k.contains("identifier") || k == "name" || k == "constant") && node.child_count() == 0
}

fn is_identifier_ref(node: &Node) -> bool {
    is_identifier(*node)
}

fn text_of(node: Node, src: &str) -> String {
    src.get(node.byte_range())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}
