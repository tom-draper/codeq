//! Haskell declaration groups, preserving every equation of a named binding.
use super::{signature::collapse_ws, text::Source, Fact};
use std::collections::HashMap;
use tree_sitter::Node;

pub(super) fn extract(root: Node, src: &[u8]) -> Vec<Fact> {
    let source = Source::new(std::str::from_utf8(src).unwrap_or(""));
    let mut facts = Vec::new();
    let mut bindings: HashMap<(usize, String), Vec<Node>> = HashMap::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "quote" | "typed_quote" | "quasiquote" | "splice" | "top_splice"
        ) {
            continue;
        }
        match node.kind() {
            "header" => {
                if let Some(name) = node.child_by_field_name("module") {
                    let mut fact = source.fact(
                        "module",
                        text(name, src),
                        node.start_byte(),
                        root.end_byte(),
                        "tree-sitter/haskell",
                    );
                    fact.signature = collapse_ws(text(node, src));
                    facts.push(fact);
                }
            }
            "import" => {
                if let Some(name) = node.child_by_field_name("module") {
                    facts.push(source.fact(
                        "import",
                        text(name, src),
                        node.start_byte(),
                        node.end_byte(),
                        "tree-sitter/haskell",
                    ));
                }
            }
            "data_type" | "newtype" | "type_synonym" | "type_family" | "data_family" | "class" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let kind = match node.kind() {
                        "class" => "class",
                        "type_synonym" => "typealias",
                        _ => "type",
                    };
                    let mut fact = source.fact(
                        kind,
                        text(name, src),
                        node.start_byte(),
                        node.end_byte(),
                        "tree-sitter/haskell",
                    );
                    fact.signature = collapse_ws(text(node, src).lines().next().unwrap_or(""));
                    facts.push(fact);
                }
            }
            "function" | "bind" | "signature" => {
                let Some(parent) = node.parent() else {
                    continue;
                };
                if !matches!(
                    parent.kind(),
                    "declarations"
                        | "local_binds"
                        | "class_declarations"
                        | "instance_declarations"
                        | "foreign_import"
                ) {
                    continue;
                }
                let mut names = Vec::new();
                if let Some(name) = node.child_by_field_name("name") {
                    names.push(name);
                }
                if let Some(list) = node.child_by_field_name("names") {
                    let mut cursor = list.walk();
                    names.extend(list.named_children(&mut cursor));
                } else if let Some(infix) = node.named_child(0).filter(|n| n.kind() == "infix") {
                    names.extend(infix.child_by_field_name("operator"));
                }
                for name in names {
                    let name = text(name, src).trim_matches(['(', ')', '`']).to_string();
                    bindings.entry((parent.id(), name)).or_default().push(node);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    for ((_, name), mut nodes) in bindings {
        nodes.sort_by_key(Node::start_byte);
        let signature = nodes.iter().find(|n| n.kind() == "signature");
        let bodies: Vec<_> = nodes.iter().filter(|n| n.kind() != "signature").collect();
        let first = bodies.first().copied().unwrap_or(&nodes[0]);
        let last = bodies.last().copied().unwrap_or(first);
        let kind = if first.kind() == "function"
            || signature
                .and_then(|n| n.child_by_field_name("type"))
                .is_some_and(|mut ty| {
                    while matches!(ty.kind(), "forall" | "context") {
                        let Some(inner) = ty.child_by_field_name("type") else {
                            return false;
                        };
                        ty = inner;
                    }
                    ty.kind() == "function"
                }) {
            "function"
        } else {
            "constant"
        };
        let range = first
            .parent()
            .filter(|p| p.kind() == "foreign_import")
            .unwrap_or(*first);
        let mut fact = source.fact(
            kind,
            &name,
            range.start_byte(),
            last.end_byte(),
            "tree-sitter/haskell",
        );
        fact.signature = if let Some(signature) = signature {
            collapse_ws(text(*signature, src))
        } else {
            let end = first
                .child_by_field_name("match")
                .map_or(first.end_byte(), |n| n.start_byte());
            collapse_ws(&String::from_utf8_lossy(&src[first.start_byte()..end]))
        };
        facts.push(fact);
    }
    facts.sort_by(|a, b| (a.start_byte, &a.name).cmp(&(b.start_byte, &b.name)));
    facts
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}
