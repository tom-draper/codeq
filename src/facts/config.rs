use super::*;

pub(super) fn config_facts(language: &str, path: &str, src: &[u8]) -> Result<Extracted> {
    let filename = path.rsplit('/').next().unwrap_or(path);
    if src.len() > 1_048_576
        || matches!(
            filename,
            "package-lock.json"
                | "composer.lock"
                | "Cargo.lock"
                | "poetry.lock"
                | "pnpm-lock.yaml"
                | "yarn.lock"
        )
    {
        return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()));
    }
    let grammar = match language {
        "json" => tree_sitter_json::LANGUAGE.into(),
        "yaml" => tree_sitter_yaml::LANGUAGE.into(),
        "toml" => tree_sitter_toml_ng::LANGUAGE.into(),
        _ => unreachable!(),
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar)?;
    let Some(tree) = parser.parse(src, None) else {
        return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()));
    };
    let root = tree.root_node();
    let mut facts = Vec::new();
    let mut spans = Vec::new();
    let mut stack = vec![(root, String::new(), 0)];
    let mut table_arrays: HashMap<String, usize> = HashMap::new();
    while let Some((node, parent, depth)) = stack.pop() {
        if depth > 64 || node.is_error() || node.is_missing() {
            continue;
        }
        if node.kind() == "comment" {
            spans.push((node.start_byte(), node.end_byte(), Span::Comment));
            continue;
        }
        if matches!(
            node.kind(),
            "string" | "double_quote_scalar" | "single_quote_scalar" | "block_scalar"
        ) {
            spans.push((node.start_byte(), node.end_byte(), Span::Str));
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| {
                if child.kind() == "comment" {
                    spans.push((child.start_byte(), child.end_byte(), Span::Comment));
                    false
                } else {
                    true
                }
            })
            .collect();
        let pair = matches!(node.kind(), "pair" | "block_mapping_pair" | "flow_pair");
        let table = matches!(node.kind(), "table" | "table_array_element");
        if pair || table {
            let key = node
                .child_by_field_name("key")
                .or_else(|| children.first().copied());
            let Some(key) = key else {
                continue;
            };
            if key.has_error() {
                continue;
            }
            let raw = key.utf8_text(src).unwrap_or_default();
            // YAML complex keys cannot be represented by a scalar key path.
            if language == "yaml" && (raw.contains('\n') || raw.starts_with(['{', '['])) {
                continue;
            }
            let suffix = if language == "toml" {
                toml_key(key, src)
            } else {
                component(raw)
            };
            let mut name = join(&parent, &suffix);
            if node.kind() == "table_array_element" {
                let index = table_arrays.entry(name.clone()).or_default();
                name = format!("{name}[{index}]");
                *index += 1;
            }
            facts.push(Fact {
                kind: "key".into(),
                is_derived: false,
                name: name.clone(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                start_line: node.start_position().row + 1,
                start_col: node.start_position().column,
                end_line: node.end_position().row + 1,
                end_col: node.end_position().column,
                signature: cap(
                    &collapse_ws(
                        node.utf8_text(src)
                            .unwrap_or_default()
                            .lines()
                            .next()
                            .unwrap_or_default(),
                    ),
                    MAX_SIG,
                ),
                doc: None,
                source: format!("tree-sitter/{language}"),
                confidence: 1.0,
                visibility: None,
                owner: (!parent.is_empty()).then_some(parent.clone()),
                shape: None,
                shape_nodes: 0,
                is_test: false,
                decorator: None,
                contract: None,
            });
            for child in children.into_iter().rev().filter(|c| c.id() != key.id()) {
                stack.push((child, name.clone(), depth + 1));
            }
            continue;
        }
        let array = matches!(node.kind(), "array" | "block_sequence" | "flow_sequence");
        for (index, child) in children
            .into_iter()
            .filter(|c| c.kind() != "comment")
            .enumerate()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let name = if array {
                format!("{parent}[{index}]")
            } else {
                parent.clone()
            };
            stack.push((child, name, depth + 1));
        }
    }
    facts.sort_by_key(|f| f.start_byte);
    spans.sort_by_key(|s| s.0);
    Ok((
        (facts, spans, Vec::new()),
        Some(parse_health(root)),
        Vec::new(),
    ))
}

fn component(raw: &str) -> String {
    let raw = raw.trim();
    let value = serde_json::from_str::<String>(raw).unwrap_or_else(|_| {
        raw.strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .map(|s| s.replace("''", "'"))
            .unwrap_or_else(|| raw.to_string())
    });
    if value.is_empty() || value.contains(['.', '[', ']', '"', '\\']) {
        format!("[{}]", serde_json::to_string(&value).unwrap())
    } else {
        value
    }
}

fn join(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else if key.starts_with('[') {
        format!("{parent}{key}")
    } else {
        format!("{parent}.{key}")
    }
}

fn toml_key(node: Node, src: &[u8]) -> String {
    if node.kind() != "dotted_key" {
        return component(node.utf8_text(src).unwrap_or_default());
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .fold(String::new(), |path, key| join(&path, &toml_key(key, src)))
}
