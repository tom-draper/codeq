use super::{
    cap, collapse_ws, parse_health, text::Source, Extracted, Fact, Relation, Span, MAX_SIG,
};
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Node, Parser};

pub(super) fn extract(src: &[u8]) -> Result<Extracted> {
    if src.len() > 1_048_576 {
        return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()));
    }
    let text = std::str::from_utf8(src)?;
    let source = Source::new(text);
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_cmake::LANGUAGE.into())?;
    let tree = parser
        .parse(src, None)
        .context("CMake parser returned no tree")?;
    let mut facts: Vec<Fact> = Vec::new();
    let mut spans = Vec::new();
    let mut relations = Vec::new();
    let mut stack = vec![(tree.root_node(), 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 {
            continue;
        }
        match node.kind() {
            "line_comment" | "bracket_comment" => {
                spans.push((node.start_byte(), node.end_byte(), Span::Comment));
                continue;
            }
            "quoted_argument" | "bracket_argument" => {
                spans.push((node.start_byte(), node.end_byte(), Span::Str));
                continue;
            }
            "function_def" | "macro_def" if !node.has_error() => {
                let command = node.named_child(0).unwrap();
                if let Some(name) = arguments(command, src).first().and_then(Option::as_deref) {
                    let kind = if node.kind() == "function_def" {
                        "function"
                    } else {
                        "macro"
                    };
                    let mut fact = source.fact(
                        kind,
                        name,
                        node.start_byte(),
                        node.end_byte(),
                        "tree-sitter/cmake",
                    );
                    fact.signature = cap(&collapse_ws(command.utf8_text(src)?), MAX_SIG);
                    facts.push(fact);
                }
            }
            "normal_command" if !node.has_error() => {
                let command = node
                    .named_child(0)
                    .unwrap()
                    .utf8_text(src)?
                    .to_ascii_lowercase();
                let args = arguments(node, src);
                if let Some(name) = args.first().and_then(Option::as_deref) {
                    match command.as_str() {
                        "set" | "option" | "add_library" | "add_executable"
                        | "add_custom_target" => {
                            let kind = if matches!(command.as_str(), "set" | "option") {
                                "key"
                            } else {
                                "target"
                            };
                            let mut fact = source.fact(
                                kind,
                                name,
                                node.start_byte(),
                                node.end_byte(),
                                "tree-sitter/cmake",
                            );
                            if kind == "target" {
                                fact.signature = format!("{command}({name})");
                            }
                            facts.push(fact);
                            if matches!(command.as_str(), "add_library" | "add_executable")
                                && args.get(1).and_then(Option::as_deref) == Some("ALIAS")
                            {
                                if let Some(parent) = args.get(2).and_then(Option::as_deref) {
                                    relations.push(Relation {
                                        subtype: name.into(),
                                        supertype: parent.into(),
                                        relation: "depends-on",
                                        start_line: source.line(node.start_byte()),
                                    });
                                }
                            }
                        }
                        "include" if name.ends_with(".cmake") => {
                            if let Some(fact) = source.file_import(
                                name,
                                node.start_byte(),
                                node.end_byte(),
                                "tree-sitter/cmake",
                            ) {
                                facts.push(fact);
                            }
                        }
                        "add_subdirectory" => {
                            if let Some(fact) = source.file_import(
                                &format!("{}/CMakeLists.txt", name.trim_end_matches('/')),
                                node.start_byte(),
                                node.end_byte(),
                                "tree-sitter/cmake",
                            ) {
                                facts.push(fact);
                            }
                        }
                        "add_dependencies" | "target_link_libraries" => {
                            for parent in args.iter().skip(1).filter_map(Option::as_deref) {
                                if command == "add_dependencies"
                                    || !matches!(
                                        parent,
                                        "PUBLIC"
                                            | "PRIVATE"
                                            | "INTERFACE"
                                            | "LINK_PUBLIC"
                                            | "LINK_PRIVATE"
                                            | "LINK_INTERFACE_LIBRARIES"
                                            | "debug"
                                            | "optimized"
                                            | "general"
                                    )
                                {
                                    relations.push(Relation {
                                        subtype: name.into(),
                                        supertype: parent.into(),
                                        relation: "depends-on",
                                        start_line: source.line(node.start_byte()),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor).map(|n| (n, depth + 1)));
        if facts.len() > 10_000 || relations.len() > 10_000 {
            facts.clear();
            spans.clear();
            relations.clear();
            break;
        }
    }
    let targets: HashSet<_> = facts
        .iter()
        .filter(|f| f.kind == "target")
        .map(|f| f.name.as_str())
        .collect();
    relations
        .retain(|r| targets.contains(r.subtype.as_str()) && targets.contains(r.supertype.as_str()));
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    Ok((
        (facts, spans, relations),
        Some(parse_health(tree.root_node())),
        Vec::new(),
    ))
}

fn arguments(node: Node<'_>, src: &[u8]) -> Vec<Option<String>> {
    let mut cursor = node.walk();
    let Some(list) = node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "argument_list")
    else {
        return Vec::new();
    };
    list.named_children(&mut cursor)
        .filter(|n| n.kind() == "argument")
        .map(|arg| {
            let value = arg.named_child(0)?;
            let raw = value.utf8_text(src).ok()?;
            let raw = match value.kind() {
                "quoted_argument" => raw.strip_prefix('"')?.strip_suffix('"')?,
                "bracket_argument" => {
                    let mut c = value.walk();
                    let content = value
                        .named_children(&mut c)
                        .find(|n| n.kind() == "bracket_argument_content")?;
                    content.utf8_text(src).ok()?
                }
                _ => raw,
            };
            (!raw.is_empty() && !raw.contains(['$', ';', '\\', '\n', '\r', '<', '>']))
                .then(|| raw.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn include_paths_preserve_filesystem_characters() {
        let ((facts, _, _), _, _) =
            super::extract(b"include(\"helpers#debug.cmake\")\ninclude(\"helpers?debug.cmake\")\n")
                .unwrap();
        let imports: Vec<_> = facts
            .iter()
            .filter(|f| f.kind == "import")
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(imports, ["./helpers#debug.cmake", "./helpers?debug.cmake"]);
    }
    #[test]
    fn dependency_target_names_are_not_link_keywords() {
        let ((_, _, relations), _, _) = super::extract(
            b"add_custom_target(PRIVATE)\nadd_custom_target(app)\nadd_dependencies(app PRIVATE)\nadd_custom_target(deploy ALIAS app)\n",
        )
        .unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].subtype, "app");
        assert_eq!(relations[0].supertype, "PRIVATE");
    }
}
