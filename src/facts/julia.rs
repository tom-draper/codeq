//! Julia declarations and imports, without evaluating macros or loading code.
use super::{signature::collapse_ws, text::Source, Fact};
use tree_sitter::Node;

pub(super) fn extract(root: Node, src: &[u8]) -> Vec<Fact> {
    let source = Source::new(std::str::from_utf8(src).unwrap_or(""));
    let mut facts = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "quote_statement" | "quote_expression") {
            continue;
        }
        if let Some((kind, name, header)) = declaration(node) {
            let mut fact = source.fact(
                kind,
                text(name, src),
                node.start_byte(),
                node.end_byte(),
                "tree-sitter/julia",
            );
            fact.signature = collapse_ws(&String::from_utf8_lossy(
                &src[node.start_byte()..header.end_byte()],
            ));
            facts.push(fact);
        }
        match node.kind() {
            "using_statement" | "import_statement" => {
                let mut cursor = node.walk();
                for item in node.named_children(&mut cursor) {
                    let module = match item.kind() {
                        "selected_import" | "import_alias" => item.named_child(0),
                        _ => Some(item),
                    };
                    if let Some(module) = module.filter(|n| static_module(*n)) {
                        facts.push(source.fact(
                            "import",
                            text(module, src),
                            node.start_byte(),
                            node.end_byte(),
                            "tree-sitter/julia",
                        ));
                    }
                }
            }
            "call_expression" => {
                if let Some(fact) = include(node, src, &source) {
                    facts.push(fact);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    facts
}

fn declaration(node: Node) -> Option<(&'static str, Node, Node)> {
    match node.kind() {
        "module_definition" => {
            let name = node.child_by_field_name("name")?;
            (name.kind() == "identifier").then_some(("module", name, name))
        }
        "function_definition" | "macro_definition" => {
            let header = node.named_child(0)?;
            let name = function_name(header, true)?;
            Some((
                if node.kind() == "macro_definition" {
                    "macro"
                } else {
                    "function"
                },
                name,
                header,
            ))
        }
        "assignment" => {
            let header = node.named_child(0)?;
            Some(("function", function_name(header, false)?, header))
        }
        "struct_definition" | "abstract_definition" | "primitive_definition" => {
            let header = node.named_child(0)?;
            let mut name = header;
            while matches!(
                name.kind(),
                "type_head" | "parametrized_type_expression" | "binary_expression"
            ) {
                name = name.named_child(0)?;
            }
            (name.kind() == "identifier").then_some((
                if node.kind() == "struct_definition" {
                    "struct"
                } else {
                    "type"
                },
                name,
                header,
            ))
        }
        _ => None,
    }
}

fn function_name(mut node: Node, allow_bare: bool) -> Option<Node> {
    while matches!(
        node.kind(),
        "signature" | "where_expression" | "typed_expression"
    ) {
        node = node.named_child(0)?;
    }
    if node.kind() == "call_expression" {
        node = node.named_child(0)?;
        if node.kind() == "parametrized_type_expression" {
            node = node.named_child(0)?;
        }
    } else if !allow_bare {
        return None;
    }
    match node.kind() {
        "identifier" | "operator" => Some(node),
        "field_expression" => {
            let mut pending = vec![node];
            while let Some(child) = pending.pop() {
                if !matches!(
                    child.kind(),
                    "field_expression" | "identifier" | "operator" | "quote_expression"
                ) {
                    return None;
                }
                let mut cursor = child.walk();
                pending.extend(child.named_children(&mut cursor));
            }
            Some(node)
        }
        _ => None,
    }
}

fn static_module(node: Node) -> bool {
    let mut pending = vec![node];
    while let Some(child) = pending.pop() {
        if !matches!(
            child.kind(),
            "identifier" | "scoped_identifier" | "import_path"
        ) {
            return false;
        }
        let mut cursor = child.walk();
        pending.extend(child.named_children(&mut cursor));
    }
    true
}

fn include(node: Node, src: &[u8], source: &Source) -> Option<Fact> {
    let callee = node.named_child(0)?;
    if callee.kind() != "identifier" || text(callee, src) != "include" {
        return None;
    }
    let args = node.named_child(1)?;
    if args.kind() != "argument_list" || args.named_child_count() != 1 {
        return None;
    }
    let value = args.named_child(0)?;
    if value.kind() != "string_literal" {
        return None;
    }
    let mut cursor = value.walk();
    if value
        .named_children(&mut cursor)
        .any(|n| n.kind() != "content")
    {
        return None;
    }
    let raw = text(value, src);
    let target = if raw.starts_with("\"\"\"") {
        return None;
    } else {
        raw.strip_prefix('"')?.strip_suffix('"')?
    };
    source.file_import(
        target,
        node.start_byte(),
        node.end_byte(),
        "tree-sitter/julia",
    )
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}
