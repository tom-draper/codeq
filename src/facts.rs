//! Syntactic fact extraction via Tree-sitter.
//!
//! v1 facts are *definitions*: named nodes we can locate reliably from syntax
//! alone (functions, structs, classes, …). Every fact carries a byte + line
//! range so an agent can fetch the exact code without rereading the file, an
//! extraction `source`, a `confidence`, plus two cheap-understanding fields:
//!
//! - `signature`: the declaration with its body stripped (e.g.
//!   `fn build() -> Widget`, `class Foo(Base):`) so an agent can grasp an API
//!   surface without reading a single body.
//! - `doc`: the leading doc comment (`///` in Rust) or docstring (first string
//!   in a Python body), collapsed and capped.

use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// Cap signatures/docs so a pathological declaration can't blow the budget.
const MAX_SIG: usize = 200;
const MAX_DOC: usize = 240;

/// A single extracted definition, before it is assigned a content-addressed id
/// (which needs the path, known only at store time).
pub struct Fact {
    pub kind: String,
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub signature: String,
    pub doc: Option<String>,
    pub source: String,
    pub confidence: f64,
}

impl Fact {
    /// Content-addressed id, stable within an index version + working-tree
    /// state. Encodes position, so it is deliberately *not* durable across
    /// edits — see the fact-model note in DRAFT.md.
    pub fn id(&self, path: &str) -> String {
        let mut h = blake3::Hasher::new();
        h.update(path.as_bytes());
        h.update(&[0]);
        h.update(self.kind.as_bytes());
        h.update(&[0]);
        h.update(self.name.as_bytes());
        h.update(&[0]);
        h.update(self.start_byte.to_le_bytes().as_slice());
        h.update(self.end_byte.to_le_bytes().as_slice());
        h.finalize().to_hex()[..16].to_string()
    }
}

/// Holds a parser + compiled query per supported language, reused across files
/// within a single refresh pass.
pub struct Extractor {
    rust: LangSpec,
    python: LangSpec,
}

struct LangSpec {
    parser: Parser,
    query: Query,
    capture_names: Vec<String>,
    source: String,
    lang: &'static str,
}

impl LangSpec {
    fn new(
        language: tree_sitter::Language,
        query_src: &str,
        source: &str,
        lang: &'static str,
    ) -> Result<LangSpec> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("setting tree-sitter language")?;
        let query = Query::new(&language, query_src).context("compiling query")?;
        let capture_names = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        Ok(LangSpec {
            parser,
            query,
            capture_names,
            source: source.to_string(),
            lang,
        })
    }

    fn extract(&mut self, src: &[u8]) -> Vec<Fact> {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut cursor = QueryCursor::new();
        let mut facts = Vec::new();
        let mut it = cursor.matches(&self.query, tree.root_node(), src);
        while let Some(m) = it.next() {
            let mut name: Option<String> = None;
            let mut kind: Option<&str> = None;
            let mut def: Option<Node> = None;
            let mut body: Option<Node> = None;
            for cap in m.captures {
                match self.capture_names[cap.index as usize].as_str() {
                    "name" => {
                        name = cap.node.utf8_text(src).ok().map(|s| s.to_string());
                    }
                    "body" => body = Some(cap.node),
                    other => {
                        kind = Some(other);
                        def = Some(cap.node);
                    }
                }
            }
            if let (Some(name), Some(kind), Some(node)) = (name, kind, def) {
                let start = node.start_position();
                let end = node.end_position();
                facts.push(Fact {
                    kind: kind.to_string(),
                    name,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    start_line: start.row + 1, // 1-based lines for humans/agents
                    start_col: start.column,
                    end_line: end.row + 1,
                    end_col: end.column,
                    signature: signature_of(src, node, body),
                    doc: doc_of(src, self.lang, node, body),
                    source: self.source.clone(),
                    confidence: 1.0,
                });
            }
        }
        facts
    }
}

impl Extractor {
    pub fn new() -> Result<Extractor> {
        Ok(Extractor {
            rust: LangSpec::new(
                tree_sitter_rust::LANGUAGE.into(),
                RUST_QUERY,
                "treesitter/rust",
                "rust",
            )?,
            python: LangSpec::new(
                tree_sitter_python::LANGUAGE.into(),
                PYTHON_QUERY,
                "treesitter/python",
                "python",
            )?,
        })
    }

    /// Extract definition facts for a file's `src`. Unsupported languages yield
    /// no facts (not an error).
    pub fn extract(&mut self, language: &str, src: &[u8]) -> Vec<Fact> {
        match language {
            "rust" => self.rust.extract(src),
            "python" => self.python.extract(src),
            _ => Vec::new(),
        }
    }
}

/// The declaration text with any body stripped, collapsed to one line and
/// capped. When there is no body capture (structs, consts, imports, …) the
/// whole node text is used, still capped.
fn signature_of(src: &[u8], def: Node, body: Option<Node>) -> String {
    let end = match body {
        Some(b) => b.start_byte(),
        None => def.end_byte(),
    };
    let raw = &src[def.start_byte()..end.min(src.len())];
    cap(&collapse_ws(&String::from_utf8_lossy(raw)), MAX_SIG)
}

fn doc_of(src: &[u8], lang: &str, def: Node, body: Option<Node>) -> Option<String> {
    let raw = match lang {
        "python" => python_doc(src, body?),
        "rust" => rust_doc(src, def),
        _ => None,
    }?;
    let s = cap(&collapse_ws(&raw), MAX_DOC);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Python docstring: the first statement in a body if it is a bare string.
fn python_doc(src: &[u8], body: Node) -> Option<String> {
    let first = body.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let expr = first.named_child(0)?;
    if expr.kind() != "string" {
        return None;
    }
    let text = expr.utf8_text(src).ok()?;
    Some(text.trim_matches(|c| c == '"' || c == '\'').to_string())
}

/// Rust doc comments: contiguous `///` lines immediately preceding the item,
/// skipping attribute lines between them and the item.
fn rust_doc(src: &[u8], def: Node) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut sib = def.prev_sibling();
    while let Some(node) = sib {
        match node.kind() {
            "line_comment" => {
                let t = node.utf8_text(src).ok()?;
                if let Some(rest) = t.strip_prefix("///") {
                    lines.push(rest.trim().to_string());
                    sib = node.prev_sibling();
                    continue;
                }
                break;
            }
            "attribute_item" => {
                sib = node.prev_sibling();
                continue;
            }
            _ => break,
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join(" "))
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

/// Each pattern binds `@name` (the identifier), one `@<kind>` capture on the
/// whole definition node, and optionally `@body` (stripped from signatures).
const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name body: (block) @body) @function
(struct_item name: (type_identifier) @name) @struct
(enum_item name: (type_identifier) @name) @enum
(union_item name: (type_identifier) @name) @union
(trait_item name: (type_identifier) @name body: (declaration_list) @body) @trait
(mod_item name: (identifier) @name) @module
(const_item name: (identifier) @name) @constant
(static_item name: (identifier) @name) @constant
(type_item name: (type_identifier) @name) @typealias
(macro_definition name: (identifier) @name) @macro

; Raw syntactic imports: name is the module path as written in source.
(use_declaration argument: (_) @name) @import
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (identifier) @name body: (block) @body) @function
(class_definition name: (identifier) @name body: (block) @body) @class

; Raw syntactic imports.
(import_statement (dotted_name) @name) @import
(import_statement (aliased_import (dotted_name) @name)) @import
(import_from_statement module_name: (dotted_name) @name) @import
(import_from_statement module_name: (relative_import) @name) @import
"#;

/// Syntactic class of a byte position, for `search --in`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Comment,
    Str,
}

/// Classifies byte offsets as inside a comment or string literal. Built once
/// per search; supports the languages we have parsers for.
pub struct Classifier {
    rust: ClsSpec,
    python: ClsSpec,
}

struct ClsSpec {
    parser: Parser,
    query: Query,
    names: Vec<String>,
}

impl ClsSpec {
    fn new(language: tree_sitter::Language, query_src: &str) -> Result<ClsSpec> {
        let mut parser = Parser::new();
        parser.set_language(&language)?;
        let query = Query::new(&language, query_src)?;
        let names = query.capture_names().iter().map(|s| s.to_string()).collect();
        Ok(ClsSpec {
            parser,
            query,
            names,
        })
    }

    /// Comment/string byte spans, sorted by start. Non-overlapping in practice.
    fn spans(&mut self, src: &[u8]) -> Vec<(usize, usize, Span)> {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut it = cursor.matches(&self.query, tree.root_node(), src);
        while let Some(m) = it.next() {
            for cap in m.captures {
                let span = match self.names[cap.index as usize].as_str() {
                    "comment" => Span::Comment,
                    "string" => Span::Str,
                    _ => continue,
                };
                out.push((cap.node.start_byte(), cap.node.end_byte(), span));
            }
        }
        out.sort_by_key(|(s, _, _)| *s);
        out
    }
}

impl Classifier {
    pub fn new() -> Result<Classifier> {
        Ok(Classifier {
            rust: ClsSpec::new(tree_sitter_rust::LANGUAGE.into(), RUST_SPANS)?,
            python: ClsSpec::new(tree_sitter_python::LANGUAGE.into(), PYTHON_SPANS)?,
        })
    }

    /// Spans for a file, or `None` when the language has no parser (caller must
    /// then exclude the file rather than guess).
    pub fn spans(&mut self, language: &str, src: &[u8]) -> Option<Vec<(usize, usize, Span)>> {
        match language {
            "rust" => Some(self.rust.spans(src)),
            "python" => Some(self.python.spans(src)),
            _ => None,
        }
    }
}

const RUST_SPANS: &str = r#"
(line_comment) @comment
(block_comment) @comment
(string_literal) @string
(raw_string_literal) @string
(char_literal) @string
"#;

const PYTHON_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;
