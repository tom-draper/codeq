//! HCL / Terraform. Blocks are `keyword "label"… { … }` and their identity lives
//! in the labels, not in any name node — so a query cannot pick it out: the labels
//! are a *repeated, unnamed* child, and which one is the name depends on how many
//! there are. `variable "region"` is named by its only label; `resource
//! "aws_s3_bucket" "logs"` is named by its last and typed by its first. Hence the
//! derivation below rather than a query, as with Svelte's component fact.

use tree_sitter::Node;

use super::{signature_of, Fact};

/// The first descendant of `node` with the given kind, breadth-first.
fn first_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut queue = std::collections::VecDeque::from([node]);
    while let Some(n) = queue.pop_front() {
        if n.kind() == kind && n.id() != node.id() {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            queue.push_back(child);
        }
    }
    None
}

/// The text a `string_lit` label carries, without its quotes.
fn hcl_label(node: Node, src: &[u8]) -> Option<String> {
    let mut c = node.walk();
    if let Some(inner) = node
        .children(&mut c)
        .find(|n| n.kind() == "template_literal")
    {
        return inner.utf8_text(src).ok().map(|t| t.to_string());
    }
    node.utf8_text(src)
        .ok()
        .map(|t| t.trim_matches('"').to_string())
}

/// The `body` child's attributes, as (name, value-node) pairs.
fn hcl_attrs<'a>(block: Node<'a>, src: &[u8]) -> Vec<(String, Node<'a>)> {
    let mut out = Vec::new();
    let mut c = block.walk();
    let Some(body) = block.children(&mut c).find(|n| n.kind() == "body") else {
        return out;
    };
    let mut bc = body.walk();
    for attr in body.children(&mut bc).filter(|n| n.kind() == "attribute") {
        let mut ac = attr.walk();
        let kids: Vec<Node> = attr.children(&mut ac).collect();
        let Some(name) = kids
            .iter()
            .find(|n| n.kind() == "identifier")
            .and_then(|n| n.utf8_text(src).ok())
        else {
            continue;
        };
        out.push((name.to_string(), attr));
    }
    out
}

fn hcl_fact(kind: &str, name: String, owner: Option<String>, node: Node, src: &[u8]) -> Fact {
    let start = node.start_position();
    let end = node.end_position();
    let mut c = node.walk();
    let body = node.children(&mut c).find(|n| n.kind() == "body");
    Fact {
        kind: kind.to_string(),
        is_derived: false,
        name,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: start.row + 1,
        start_col: start.column,
        end_line: end.row + 1,
        end_col: end.column,
        signature: signature_of(src, node, body),
        doc: None,
        source: "treesitter/hcl".to_string(),
        confidence: 1.0,
        visibility: None,
        owner,
        shape: None,
        shape_nodes: 0,
        is_test: false,
        decorator: None,
        contract: None,
    }
}

/// Definitions for every labelled HCL block, plus `locals` entries and the
/// `source` of a `module` block.
///
/// Naming follows how the language *refers* to things, because codeq's graph is
/// name-based: Terraform writes `aws_s3_bucket.logs.arn`, `var.region`,
/// `module.vpc.id`, so the name is the last label and the type label becomes the
/// `owner` — the same split as a struct field and its type. `kind` is the block
/// keyword verbatim, which keeps this honest for non-Terraform HCL too (Nomad's
/// `job`, Packer's `source`) at the cost of an open-ended kind vocabulary.
/// A block with no labels (`terraform`, `lifecycle`) has no identity and is skipped.
pub(super) fn hcl_facts(root: Node, src: &[u8]) -> Vec<Fact> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
        if node.kind() != "block" {
            continue;
        }
        let mut bc = node.walk();
        let kids: Vec<Node> = node.children(&mut bc).collect();
        let Some(keyword) = kids
            .iter()
            .find(|n| n.kind() == "identifier")
            .and_then(|n| n.utf8_text(src).ok())
        else {
            continue;
        };
        let labels: Vec<String> = kids
            .iter()
            .filter(|n| n.kind() == "string_lit")
            .filter_map(|n| hcl_label(*n, src))
            .collect();

        // `locals` declares one name per attribute, referenced as `local.x`.
        if keyword == "locals" {
            for (name, attr) in hcl_attrs(node, src) {
                out.push(hcl_fact("constant", name, Some("local".into()), attr, src));
            }
            continue;
        }
        let Some(name) = labels.last().cloned() else {
            continue; // unlabelled block: no identity to address it by
        };
        let owner = if labels.len() >= 2 {
            labels.first().cloned()
        } else {
            Some(keyword.to_string())
        };
        out.push(hcl_fact(keyword, name, owner, node, src));

        // A module's `source` is an import in every sense that matters here, so
        // `deps`/`importers` work across a Terraform tree.
        if keyword == "module" {
            for (attr_name, attr) in hcl_attrs(node, src) {
                if attr_name != "source" {
                    continue;
                }
                // The value nests as expression > literal_value > string_lit, so
                // search the attribute's subtree rather than assume a depth.
                if let Some(v) =
                    first_descendant(attr, "string_lit").and_then(|l| hcl_label(l, src))
                {
                    let mut f = hcl_fact("import", v, None, attr, src);
                    f.visibility = None;
                    out.push(f);
                }
            }
        }
    }
    out.sort_by_key(|f| f.start_byte);
    out
}
