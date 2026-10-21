//! OCaml declarations and syntactic module imports in implementations and interfaces.
use super::{signature::collapse_ws, text::Source, Fact};
use tree_sitter::Node;

pub(super) fn extract(root: Node, src: &[u8]) -> Vec<Fact> {
    let source = Source::new(std::str::from_utf8(src).unwrap_or(""));
    let mut facts = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "extension"
                | "item_extension"
                | "attribute_payload"
                | "quoted_extension"
                | "quoted_item_extension"
        ) {
            continue;
        }
        if let Some((kind, name)) = declaration(node) {
            let start = node
                .parent()
                .filter(|parent| {
                    matches!(
                        parent.kind(),
                        "value_definition"
                            | "module_definition"
                            | "type_definition"
                            | "class_definition"
                            | "class_type_definition"
                    ) && parent.named_child(0) == Some(node)
                })
                .map_or(node.start_byte(), |p| p.start_byte());
            let mut fact = source.fact(
                kind,
                text(name, src).trim_matches(['(', ')', ' ']),
                start,
                node.end_byte(),
                "tree-sitter/ocaml",
            );
            let end = node
                .child_by_field_name("body")
                .map_or(node.end_byte(), |body| body.start_byte());
            fact.signature = collapse_ws(&String::from_utf8_lossy(&src[start..end]));
            facts.push(fact);
        }
        let module = match node.kind() {
            "open_module" | "open_module_signature" | "include_module" => {
                node.child_by_field_name("module")
            }
            "include_module_type" => node.child_by_field_name("module_type"),
            "module_binding" => node.child_by_field_name("body"),
            _ => None,
        };
        if let Some(module) = module.filter(|m| static_module(*m)) {
            facts.push(source.fact(
                "import",
                text(module, src),
                node.start_byte(),
                node.end_byte(),
                "tree-sitter/ocaml",
            ));
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    facts
}

fn declaration(node: Node) -> Option<(&'static str, Node)> {
    let (kind, name) = match node.kind() {
        "let_binding" => {
            let mut name = node.child_by_field_name("pattern")?;
            while matches!(name.kind(), "parenthesized_pattern" | "typed_pattern") {
                name = if name.kind() == "typed_pattern" {
                    name.child_by_field_name("pattern")?
                } else {
                    name.named_child(0)?
                };
            }
            if !matches!(name.kind(), "value_name" | "parenthesized_operator") {
                return None;
            }
            let mut cursor = node.walk();
            let parameters = node
                .named_children(&mut cursor)
                .any(|n| n.kind() == "parameter");
            let mut body = node.child_by_field_name("body")?;
            while matches!(body.kind(), "parenthesized_expression" | "typed_expression") {
                body = body.child_by_field_name("expression")?;
            }
            (
                if parameters || matches!(body.kind(), "fun_expression" | "function_expression") {
                    "function"
                } else {
                    "constant"
                },
                name,
            )
        }
        "value_specification" | "external" => {
            let name = child(node, &["value_name", "parenthesized_operator"])?;
            let mut ty = node.child_by_field_name("type")?;
            while ty.kind() == "polymorphic_type" {
                ty = ty.child_by_field_name("type")?;
            }
            (
                if ty.kind() == "function_type" {
                    "function"
                } else {
                    "constant"
                },
                name,
            )
        }
        "type_binding" => ("type", node.child_by_field_name("name")?),
        "module_binding" => ("module", child(node, &["module_name"])?),
        "module_type_definition" => ("interface", child(node, &["module_type_name"])?),
        "class_binding" => ("class", child(node, &["class_name"])?),
        "class_type_binding" => ("interface", child(node, &["class_type_name"])?),
        "method_definition" | "method_specification" => ("method", child(node, &["method_name"])?),
        "exception_definition" => (
            "type",
            child(
                child(node, &["constructor_declaration"])?,
                &["constructor_name"],
            )?,
        ),
        _ => return None,
    };
    Some((kind, name))
}

fn child<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|n| kinds.contains(&n.kind()));
    found
}

fn static_module(node: Node) -> bool {
    let mut pending = vec![node];
    while let Some(child) = pending.pop() {
        if !matches!(
            child.kind(),
            "module_path"
                | "extended_module_path"
                | "module_name"
                | "module_type_path"
                | "module_type_name"
        ) {
            return false;
        }
        let mut cursor = child.walk();
        pending.extend(child.named_children(&mut cursor));
    }
    true
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}
