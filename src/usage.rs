//! Syntax evidence for textual references. Unsupported syntax stays unknown.
use std::collections::HashSet;
use tree_sitter::{Node, Tree};

pub(crate) struct Uses {
    language: String,
    tree: Option<Tree>,
    calls: HashSet<usize>,
}

impl Uses {
    pub(crate) fn parse(language: &str, body: &str) -> Self {
        let tree = crate::astgrep::ts_language(language).and_then(|grammar| {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&grammar).ok()?;
            parser.parse(body, None)
        });
        let calls = tree
            .as_ref()
            .map(|t| {
                crate::callsite::call_sites_from_tree(language, t.root_node(), body.as_bytes())
                    .into_iter()
                    .map(|c| c.start_byte)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            language: language.into(),
            tree,
            calls,
        }
    }

    pub(crate) fn classify(&self, byte: usize) -> (&'static str, String) {
        let evidence = |kind: &'static str, syntax: &str| {
            (kind, format!("tree-sitter/{}:{syntax}", self.language))
        };
        let Some(tree) = &self.tree else {
            return ("unknown", "no syntax classifier for this format".into());
        };
        let Some(leaf) = tree.root_node().descendant_for_byte_range(byte, byte + 1) else {
            return ("unknown", "no syntax node at occurrence".into());
        };
        let mut ancestors = vec![leaf];
        while let Some(parent) = ancestors.last().and_then(|n| n.parent()) {
            ancestors.push(parent);
        }
        if ancestors.iter().any(|n| n.is_error() || n.is_missing()) {
            return ("unknown", "occurrence intersects a syntax error".into());
        }
        if ancestors.iter().any(|n| {
            matches!(
                n.kind(),
                "comment"
                    | "line_comment"
                    | "block_comment"
                    | "string"
                    | "string_literal"
                    | "string_fragment"
                    | "string_content"
                    | "raw_string_literal"
            )
        }) {
            return ("unknown", "text in a comment or string".into());
        }
        if !matches!(
            self.language.as_str(),
            "rust" | "python" | "javascript" | "typescript" | "tsx"
        ) {
            return (
                "unknown",
                format!("use classification unsupported for {}", self.language),
            );
        }
        if leaf.start_byte() != byte {
            return (
                "unknown",
                "match is not a complete syntax identifier".into(),
            );
        }
        for n in &ancestors {
            if matches!(
                n.kind(),
                "import_statement"
                    | "import_from_statement"
                    | "use_declaration"
                    | "extern_crate_declaration"
                    | "import_alias"
            ) {
                return evidence("import", n.kind());
            }
        }
        if self.calls.contains(&byte) {
            return evidence("call", "callee");
        }
        for n in &ancestors {
            if matches!(
                n.kind(),
                "type_item"
                    | "type_alias_declaration"
                    | "interface_declaration"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
            ) && n
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == leaf.id())
            {
                return evidence("decl", &format!("{}.name", n.kind()));
            }
        }
        for n in &ancestors {
            if matches!(
                n.kind(),
                "type_annotation"
                    | "type_arguments"
                    | "type_identifier"
                    | "generic_type"
                    | "scoped_type_identifier"
                    | "return_type"
                    | "type_parameters"
            ) {
                return evidence("type-use", n.kind());
            }
            if matches!(
                n.kind(),
                "assignment_expression"
                    | "assignment"
                    | "augmented_assignment"
                    | "augmented_assignment_expression"
                    | "compound_assignment_expr"
                    | "update_expression"
            ) {
                let target = n
                    .child_by_field_name("left")
                    .or_else(|| n.child_by_field_name("argument"));
                if let Some(target) = target {
                    if contains(target, leaf) {
                        let writes = written_part(target, leaf);
                        let kind = if !writes {
                            "read"
                        } else if matches!(n.kind(), "assignment_expression" | "assignment") {
                            "write"
                        } else {
                            "read-write"
                        };
                        return evidence(kind, &format!("{}.target", n.kind()));
                    }
                }
            }
            let declaration_field = match n.kind() {
                "function_declaration"
                | "function_definition"
                | "function_item"
                | "method_definition"
                | "class_declaration"
                | "class_definition"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "interface_declaration"
                | "type_alias_declaration"
                | "type_item"
                | "variable_declarator" => Some("name"),
                "let_declaration" | "parameter" | "required_parameter" | "optional_parameter" => {
                    Some("pattern")
                }
                _ => None,
            };
            if let Some(field) = declaration_field {
                let target = n
                    .child_by_field_name(field)
                    .or_else(|| n.child_by_field_name("name"));
                if target.is_some_and(|target| contains(target, leaf) && written_part(target, leaf))
                {
                    return evidence("decl", &format!("{}.{}", n.kind(), field));
                }
            }
            if matches!(
                n.kind(),
                "parameters" | "formal_parameters" | "lambda_parameters"
            ) && leaf.parent().is_some_and(|p| p.id() == n.id())
            {
                return evidence("decl", n.kind());
            }
        }
        if matches!(
            leaf.kind(),
            "identifier"
                | "field_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "self"
        ) {
            if ancestors
                .iter()
                .any(|n| matches!(n.kind(), "token_tree" | "macro_invocation" | "ERROR"))
            {
                return (
                    "unknown",
                    "macro tokens have no classified use semantics".into(),
                );
            }
            if ancestors.iter().any(|n| {
                matches!(
                    n.kind(),
                    "pair" | "pair_pattern" | "object_assignment_pattern" | "match_pattern"
                )
            }) {
                return (
                    "unknown",
                    "object key or pattern use is not classified".into(),
                );
            }
            return evidence("read", leaf.kind());
        }
        ("unknown", format!("unclassified syntax {}", leaf.kind()))
    }
}

fn contains(outer: Node, inner: Node) -> bool {
    outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte()
}

fn written_part(target: Node, leaf: Node) -> bool {
    let mut node = leaf;
    while node.id() != target.id() {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "member_expression" | "attribute" | "field_expression" => {
                let field = parent
                    .child_by_field_name("property")
                    .or_else(|| parent.child_by_field_name("attribute"))
                    .or_else(|| parent.child_by_field_name("field"));
                if field.is_none_or(|f| !contains(f, leaf)) {
                    return false;
                }
            }
            // The collection and index are read when writing an element.
            "subscript_expression" | "subscript" | "index_expression" => return false,
            "pointer_expression" | "unary_expression" => return false,
            "pair_pattern" => {
                if parent
                    .child_by_field_name("key")
                    .is_some_and(|key| contains(key, leaf))
                {
                    return false;
                }
            }
            _ => {}
        }
        node = parent;
    }
    true
}
