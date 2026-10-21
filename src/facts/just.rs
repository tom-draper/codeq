use super::{
    cap, collapse_ws, parse_health, text::Source, Extracted, Fact, Relation, Span, MAX_SIG,
};
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::Parser;

pub(super) fn extract(src: &[u8]) -> Result<Extracted> {
    if src.len() > 1_048_576 {
        return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()));
    }
    let text = std::str::from_utf8(src)?;
    let source = Source::new(text);
    let mut parser = Parser::new();
    parser.set_language(&codebook_tree_sitter_just::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .context("Just parser returned no tree")?;
    let root = tree.root_node();
    let mut facts: Vec<Fact> = Vec::new();
    let mut relations = Vec::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor).filter(|n| !n.has_error()) {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok());
        let kind = match node.kind() {
            "recipe" | "alias" => Some("target"),
            "assignment" => Some("key"),
            "module" => Some("module"),
            "function_definition" => Some("function"),
            _ => None,
        };
        if let (Some(name), Some(kind)) = (name, kind) {
            let mut fact = source.fact(
                kind,
                name,
                node.start_byte(),
                node.end_byte(),
                "tree-sitter/just",
            );
            if node.kind() == "recipe" {
                let end = node
                    .child_by_field_name("body")
                    .map(|n| n.start_byte())
                    .unwrap_or(node.end_byte());
                fact.signature = cap(&collapse_ws(&text[node.start_byte()..end]), MAX_SIG);
                let mut c = node.walk();
                for dependency in node
                    .named_children(&mut c)
                    .filter(|n| n.kind() == "dependency")
                {
                    if let Some(parent) = dependency.child_by_field_name("name") {
                        relations.push(Relation {
                            subtype: name.into(),
                            supertype: parent.utf8_text(src)?.into(),
                            relation: "depends-on",
                            start_line: source.line(dependency.start_byte()),
                        });
                    }
                }
            } else if node.kind() == "alias" {
                if let Some(parent) = node.child_by_field_name("target") {
                    relations.push(Relation {
                        subtype: name.into(),
                        supertype: parent.utf8_text(src)?.into(),
                        relation: "depends-on",
                        start_line: source.line(node.start_byte()),
                    });
                }
            }
            facts.push(fact);
        }
        if matches!(node.kind(), "import" | "module") {
            if let Some(path) = node.child_by_field_name("path") {
                let raw = path.utf8_text(src)?;
                let path = ["\"\"\"", "'''", "\"", "'"]
                    .into_iter()
                    .find_map(|quote| raw.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)));
                if let Some(path) =
                    path.filter(|s| !s.contains(['\\', '\n', '\r']) && !s.starts_with('~'))
                {
                    if let Some(fact) = source.file_import(
                        path,
                        node.start_byte(),
                        node.end_byte(),
                        "tree-sitter/just",
                    ) {
                        facts.push(fact);
                    }
                }
            }
        }
        if facts.len() > 10_000 || relations.len() > 10_000 {
            return Ok((
                (Vec::new(), Vec::new(), Vec::new()),
                Some(parse_health(root)),
                Vec::new(),
            ));
        }
    }
    let targets: HashSet<_> = facts
        .iter()
        .filter(|f| f.kind == "target")
        .map(|f| f.name.as_str())
        .collect();
    relations.retain(|r| targets.contains(r.supertype.as_str()));
    let mut spans = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 {
            continue;
        }
        let span = match node.kind() {
            "comment" => Some(Span::Comment),
            "string" | "format_string" | "backtick" => Some(Span::Str),
            "recipe_body" => continue,
            _ => None,
        };
        if let Some(span) = span {
            spans.push((node.start_byte(), node.end_byte(), span));
            continue;
        }
        let mut c = node.walk();
        stack.extend(node.named_children(&mut c).map(|n| (n, depth + 1)));
    }
    Ok((
        (facts, spans, relations),
        Some(parse_health(root)),
        Vec::new(),
    ))
}
