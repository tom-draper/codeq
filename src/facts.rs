//! Syntactic fact extraction via Tree-sitter.
//!
//! v1 facts are *definitions*: named nodes we can locate reliably from syntax
//! alone (functions, structs, classes, …). Every fact carries a byte + line
//! range so an agent can fetch the exact code without rereading the file, an
//! extraction `source`, and a `confidence`. Definitions from a Tree-sitter
//! query are high confidence; heuristic facts (later) will be lower.

use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

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
}

impl LangSpec {
    fn new(
        language: tree_sitter::Language,
        query_src: &str,
        source: &str,
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
            for cap in m.captures {
                let cname = self.capture_names[cap.index as usize].as_str();
                if cname == "name" {
                    name = cap.node.utf8_text(src).ok().map(|s| s.to_string());
                } else {
                    kind = Some(cname);
                    def = Some(cap.node);
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
            )?,
            python: LangSpec::new(
                tree_sitter_python::LANGUAGE.into(),
                PYTHON_QUERY,
                "treesitter/python",
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

/// Each pattern binds `@name` (the identifier) and one `@<kind>` capture on the
/// whole definition node; the capture name becomes the fact kind.
const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name) @function
(struct_item name: (type_identifier) @name) @struct
(enum_item name: (type_identifier) @name) @enum
(union_item name: (type_identifier) @name) @union
(trait_item name: (type_identifier) @name) @trait
(mod_item name: (identifier) @name) @module
(const_item name: (identifier) @name) @constant
(static_item name: (identifier) @name) @constant
(type_item name: (type_identifier) @name) @typealias
(macro_definition name: (identifier) @name) @macro

; Raw syntactic imports: name is the module path as written in source.
(use_declaration argument: (_) @name) @import
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (identifier) @name) @function
(class_definition name: (identifier) @name) @class

; Raw syntactic imports.
(import_statement (dotted_name) @name) @import
(import_statement (aliased_import (dotted_name) @name)) @import
(import_from_statement module_name: (dotted_name) @name) @import
(import_from_statement module_name: (relative_import) @name) @import
"#;
