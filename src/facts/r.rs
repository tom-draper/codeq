//! R functions assigned to names and statically spelled loading calls.
use super::{signature::collapse_ws, text::Source, Fact};
use tree_sitter::Node;

pub(super) fn extract(root: Node, src: &[u8]) -> Vec<Fact> {
    let source = Source::new(std::str::from_utf8(src).unwrap_or(""));
    let mut facts = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "binary_operator" | "unary_operator")
            && node
                .child_by_field_name("operator")
                .is_some_and(|n| text(n, src) == "~")
        {
            continue;
        }
        if node.kind() == "binary_operator" {
            if let Some(fact) = function(node, src, &source) {
                facts.push(fact);
            }
        }
        if node.kind() == "call" {
            let callee = node
                .child_by_field_name("function")
                .map(|mut callee| {
                    if callee.kind() == "namespace_operator"
                        && callee
                            .child_by_field_name("lhs")
                            .is_some_and(|n| text(n, src).trim_matches('`') == "base")
                    {
                        if let Some(name) = callee.child_by_field_name("rhs") {
                            callee = name;
                        }
                    }
                    text(callee, src).trim_matches(['`', '\'', '"'])
                })
                .unwrap_or("");
            if matches!(
                callee,
                "quote" | "expression" | "substitute" | "alist" | "bquote"
            ) {
                continue;
            }
            if let Some(fact) = import(node, callee, src, &source) {
                facts.push(fact);
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    facts
}

fn function(node: Node, src: &[u8], source: &Source) -> Option<Fact> {
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;
    let (name, mut value) = match text(node.child_by_field_name("operator")?, src) {
        "<-" | "<<-" | "=" => (lhs, rhs),
        "->" | "->>" => (rhs, lhs),
        _ => return None,
    };
    while value.kind() == "parenthesized_expression" {
        value = value.named_child(0)?;
    }
    if name.kind() != "identifier" || value.kind() != "function_definition" {
        return None;
    }
    let name = text(name, src).trim_matches('`');
    if name.contains('\\') {
        return None;
    }
    let body = value.child_by_field_name("body")?;
    let mut fact = source.fact(
        "function",
        name,
        node.start_byte(),
        node.end_byte(),
        "tree-sitter/r",
    );
    fact.signature = format!(
        "{name} <- {}",
        collapse_ws(&String::from_utf8_lossy(
            &src[value.start_byte()..body.start_byte()]
        ))
    );
    Some(fact)
}

fn import(node: Node, callee: &str, src: &[u8], source: &Source) -> Option<Fact> {
    let file = matches!(callee, "source" | "sys.source");
    if !file
        && !matches!(
            callee,
            "library" | "require" | "requireNamespace" | "loadNamespace"
        )
    {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let args: Vec<_> = args
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "argument")
        .collect();
    let parameter = if file { "file" } else { "package" };
    if args.iter().any(|a| {
        a.child_by_field_name("name").is_some_and(|n| {
            let name = text(n, src).trim_matches(['`', '\'', '"']);
            name != parameter && parameter.starts_with(name)
        })
    }) {
        return None;
    }
    let value = args
        .iter()
        .find(|a| {
            a.child_by_field_name("name")
                .is_some_and(|n| text(n, src).trim_matches(['`', '\'', '"']) == parameter)
        })
        .or_else(|| {
            args.iter()
                .find(|a| a.child_by_field_name("name").is_none())
        })?
        .child_by_field_name("value")?;
    let target = if value.kind() == "string" {
        let content = value.child_by_field_name("content")?;
        let mut cursor = content.walk();
        if content
            .named_children(&mut cursor)
            .any(|n| n.kind() == "escape_sequence")
        {
            return None;
        }
        text(content, src)
    } else if !file && matches!(callee, "library" | "require") && value.kind() == "identifier" {
        if args.iter().any(|a| match a.child_by_field_name("name") {
            None => a.child_by_field_name("value") != Some(value),
            Some(name) => {
                "character.only".starts_with(text(name, src).trim_matches(['`', '\'', '"']))
                    && a.child_by_field_name("value")
                        .is_none_or(|n| text(n, src) != "FALSE")
            }
        }) {
            return None;
        }
        text(value, src).trim_matches('`')
    } else {
        return None;
    };
    if target.is_empty() {
        return None;
    }
    if file {
        source.file_import(target, node.start_byte(), node.end_byte(), "tree-sitter/r")
    } else {
        Some(source.fact(
            "import",
            target,
            node.start_byte(),
            node.end_byte(),
            "tree-sitter/r",
        ))
    }
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}
