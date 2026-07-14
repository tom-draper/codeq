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
    javascript: LangSpec,
    typescript: LangSpec,
    tsx: LangSpec,
    go: LangSpec,
    java: LangSpec,
    ruby: LangSpec,
    c: LangSpec,
    cpp: LangSpec,
    csharp: LangSpec,
    php: LangSpec,
}

struct LangSpec {
    parser: Parser,
    query: Query,
    capture_names: Vec<String>,
    span_query: Query,
    span_capture_names: Vec<String>,
    source: String,
    lang: &'static str,
}

impl LangSpec {
    fn new(
        language: tree_sitter::Language,
        query_src: &str,
        span_src: &str,
        source: &str,
        lang: &'static str,
    ) -> Result<LangSpec> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("setting tree-sitter language")?;
        let query = Query::new(&language, query_src).context("compiling query")?;
        let capture_names = query.capture_names().iter().map(|s| s.to_string()).collect();
        let span_query = Query::new(&language, span_src).context("compiling span query")?;
        let span_capture_names = span_query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        Ok(LangSpec {
            parser,
            query,
            capture_names,
            span_query,
            span_capture_names,
            source: source.to_string(),
            lang,
        })
    }

    /// One parse yields both definition facts and comment/string spans.
    fn extract(&mut self, src: &[u8]) -> (Vec<Fact>, Vec<(usize, usize, Span)>) {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return (Vec::new(), Vec::new()),
        };
        let root = tree.root_node();

        let mut facts = Vec::new();
        {
            let mut cursor = QueryCursor::new();
            let mut it = cursor.matches(&self.query, root, src);
            while let Some(m) = it.next() {
                let mut name: Option<String> = None;
                let mut kind: Option<&str> = None;
                let mut def: Option<Node> = None;
                let mut body: Option<Node> = None;
                let mut guard: Option<&str> = None;
                for cap in m.captures {
                    match self.capture_names[cap.index as usize].as_str() {
                        "name" => {
                            name = cap.node.utf8_text(src).ok().map(|s| s.to_string());
                        }
                        "body" => body = Some(cap.node),
                        // `@guard` marks a token the match is only valid for when
                        // it equals a known keyword — e.g. Ruby `require`, which
                        // is a plain method call the grammar can't distinguish.
                        // (The query engine here doesn't evaluate `#eq?`, so we
                        // filter in code.)
                        "guard" => guard = cap.node.utf8_text(src).ok(),
                        other => {
                            kind = Some(other);
                            def = Some(cap.node);
                        }
                    }
                }
                if let Some(g) = guard {
                    if !matches!(g, "require" | "require_relative"
                        | "require_dependency" | "load" | "autoload")
                    {
                        continue;
                    }
                }
                if let (Some(name), Some(kind), Some(node)) = (name, kind, def) {
                    // Some grammars capture the whole quoted import path (Go);
                    // normalise to the bare module string like the others.
                    let name = if kind == "import" {
                        name.trim_matches(|c: char| "\"'`<>".contains(c))
                            .to_string()
                    } else {
                        name
                    };
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
        }

        let mut spans = Vec::new();
        {
            let mut cursor = QueryCursor::new();
            let mut it = cursor.matches(&self.span_query, root, src);
            while let Some(m) = it.next() {
                for cap in m.captures {
                    let span = match self.span_capture_names[cap.index as usize].as_str() {
                        "comment" => Span::Comment,
                        "string" => Span::Str,
                        _ => continue,
                    };
                    spans.push((cap.node.start_byte(), cap.node.end_byte(), span));
                }
            }
            spans.sort_by_key(|(s, _, _)| *s);
        }

        (facts, spans)
    }
}

impl Extractor {
    pub fn new() -> Result<Extractor> {
        Ok(Extractor {
            rust: LangSpec::new(
                tree_sitter_rust::LANGUAGE.into(),
                RUST_QUERY,
                RUST_SPANS,
                "treesitter/rust",
                "rust",
            )?,
            python: LangSpec::new(
                tree_sitter_python::LANGUAGE.into(),
                PYTHON_QUERY,
                PYTHON_SPANS,
                "treesitter/python",
                "python",
            )?,
            javascript: LangSpec::new(
                tree_sitter_javascript::LANGUAGE.into(),
                JS_QUERY,
                JS_SPANS,
                "treesitter/javascript",
                "javascript",
            )?,
            typescript: LangSpec::new(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                &format!("{JS_QUERY}\n{TS_EXTRA}"),
                JS_SPANS,
                "treesitter/typescript",
                "typescript",
            )?,
            tsx: LangSpec::new(
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                &format!("{JS_QUERY}\n{TS_EXTRA}"),
                JS_SPANS,
                "treesitter/tsx",
                "tsx",
            )?,
            go: LangSpec::new(
                tree_sitter_go::LANGUAGE.into(),
                GO_QUERY,
                GO_SPANS,
                "treesitter/go",
                "go",
            )?,
            java: LangSpec::new(
                tree_sitter_java::LANGUAGE.into(),
                JAVA_QUERY,
                JAVA_SPANS,
                "treesitter/java",
                "java",
            )?,
            ruby: LangSpec::new(
                tree_sitter_ruby::LANGUAGE.into(),
                RUBY_QUERY,
                RUBY_SPANS,
                "treesitter/ruby",
                "ruby",
            )?,
            c: LangSpec::new(
                tree_sitter_c::LANGUAGE.into(),
                C_QUERY,
                C_SPANS,
                "treesitter/c",
                "c",
            )?,
            cpp: LangSpec::new(
                tree_sitter_cpp::LANGUAGE.into(),
                CPP_QUERY,
                CPP_SPANS,
                "treesitter/cpp",
                "cpp",
            )?,
            csharp: LangSpec::new(
                tree_sitter_c_sharp::LANGUAGE.into(),
                CSHARP_QUERY,
                CSHARP_SPANS,
                "treesitter/csharp",
                "csharp",
            )?,
            php: LangSpec::new(
                tree_sitter_php::LANGUAGE_PHP.into(),
                PHP_QUERY,
                PHP_SPANS,
                "treesitter/php",
                "php",
            )?,
        })
    }

    /// One parse per file yields definition facts and comment/string spans.
    /// Unsupported languages yield nothing (not an error).
    pub fn extract(&mut self, language: &str, src: &[u8]) -> (Vec<Fact>, Vec<(usize, usize, Span)>) {
        match language {
            "rust" => self.rust.extract(src),
            "python" => self.python.extract(src),
            "javascript" => self.javascript.extract(src),
            "typescript" => self.typescript.extract(src),
            "tsx" => self.tsx.extract(src),
            "go" => self.go.extract(src),
            "java" => self.java.extract(src),
            "ruby" => self.ruby.extract(src),
            "c" => self.c.extract(src),
            "cpp" => self.cpp.extract(src),
            "csharp" => self.csharp.extract(src),
            "php" => self.php.extract(src),
            _ => (Vec::new(), Vec::new()),
        }
    }
}

/// Whether codeq can classify comment/string spans for this language (i.e. it
/// has a parser). `search --in` excludes files in other languages rather than
/// guess.
pub fn has_spans(language: &str) -> bool {
    matches!(
        language,
        "rust"
            | "python"
            | "javascript"
            | "typescript"
            | "tsx"
            | "go"
            | "java"
            | "ruby"
            | "c"
            | "cpp"
            | "csharp"
            | "php"
    )
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

/// JavaScript definitions and ES imports. Node types here also exist in the
/// TypeScript grammar, so this compiles against JS, TS, and TSX. `name: (_)`
/// avoids depending on grammar-specific identifier node types.
const JS_QUERY: &str = r#"
(function_declaration name: (_) @name body: (statement_block) @body) @function
(generator_function_declaration name: (_) @name body: (statement_block) @body) @function
(class_declaration name: (_) @name body: (class_body) @body) @class
(method_definition name: (_) @name body: (statement_block) @body) @method
(variable_declarator name: (_) @name value: (arrow_function body: (_) @body)) @function
(variable_declarator name: (_) @name value: (function_expression body: (_) @body)) @function

; ES module imports and re-exports (raw module string, quotes stripped).
(import_statement source: (string (string_fragment) @name)) @import
(export_statement source: (string (string_fragment) @name)) @import
; CommonJS require("x") — a call, not a syntactic import, so it's guarded by the
; callee name (`require`) via the @guard handling in `extract`.
(call_expression
  function: (identifier) @guard
  arguments: (arguments . (string (string_fragment) @name))) @import
"#;

/// TypeScript-only definitions, appended to `JS_QUERY` for TS and TSX.
const TS_EXTRA: &str = r#"
(interface_declaration name: (_) @name) @interface
(type_alias_declaration name: (_) @name) @type
(enum_declaration name: (_) @name) @enum
"#;

/// Go definitions and imports.
const GO_QUERY: &str = r#"
(function_declaration name: (identifier) @name body: (block) @body) @function
(method_declaration name: (field_identifier) @name body: (block) @body) @method
(type_declaration (type_spec name: (type_identifier) @name type: (struct_type)) @struct)
(type_declaration (type_spec name: (type_identifier) @name type: (interface_type)) @interface)
(type_declaration (type_spec name: (type_identifier) @name type: (type_identifier)) @type)
(type_declaration (type_alias name: (type_identifier) @name) @type)
(import_spec path: (interpreted_string_literal) @name) @import
"#;

const GO_SPANS: &str = r#"
(comment) @comment
(interpreted_string_literal) @string
(raw_string_literal) @string
"#;

/// Java definitions and imports.
const JAVA_QUERY: &str = r#"
(class_declaration name: (identifier) @name body: (class_body) @body) @class
(interface_declaration name: (identifier) @name body: (interface_body) @body) @interface
(enum_declaration name: (identifier) @name body: (enum_body) @body) @enum
(record_declaration name: (identifier) @name) @record
(method_declaration name: (identifier) @name body: (block) @body) @method
(method_declaration name: (identifier) @name !body) @method
(constructor_declaration name: (identifier) @name body: (constructor_body) @body) @method
(import_declaration (scoped_identifier) @name) @import
"#;

const JAVA_SPANS: &str = r#"
(line_comment) @comment
(block_comment) @comment
(string_literal) @string
(character_literal) @string
"#;

/// Ruby definitions and `require`/`require_relative` imports. Ruby has no import
/// statement — `require "x"` is a plain method call — so imports are captured as
/// calls guarded by the method name (see the `@guard` handling in `extract`).
/// Ruby method/class bodies aren't a strippable field here, so signatures are
/// the whole declaration, capped.
const RUBY_QUERY: &str = r#"
(method name: (_) @name) @method
(singleton_method name: (_) @name) @method
(class name: (_) @name) @class
(module name: (_) @name) @module
(call
  method: (identifier) @guard
  arguments: (argument_list . (string (string_content) @name))) @import
"#;

const RUBY_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;

/// C definitions and #include directives. Declarator nesting means some forms
/// (e.g. pointer-returning functions) need their own pattern.
const C_QUERY: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name) body: (compound_statement) @body) @function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)) body: (compound_statement) @body) @function
(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @enum
(preproc_include path: (system_lib_string) @name) @import
(preproc_include path: (string_literal) @name) @import
"#;

const C_SPANS: &str = r#"
(comment) @comment
(string_literal) @string
(char_literal) @string
"#;

/// C++ (a superset of C): add classes, namespaces, and member functions.
const CPP_QUERY: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name) body: (compound_statement) @body) @function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)) body: (compound_statement) @body) @function
(function_definition declarator: (function_declarator declarator: (field_identifier) @name) body: (compound_statement) @body) @method
(function_definition declarator: (function_declarator declarator: (qualified_identifier) @name) body: (compound_statement) @body) @method
; Declaration-only members (the norm in headers): method/constructor prototypes
; with no body. Without these, a header-only class exposes none of its methods.
(field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @method
(field_declaration declarator: (function_declarator declarator: (identifier) @name)) @method
; Free-function prototypes at file scope (also headers).
(declaration declarator: (function_declarator declarator: (identifier) @name)) @function
(class_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @class
(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @enum
(namespace_definition name: (namespace_identifier) @name body: (declaration_list) @body) @namespace
(preproc_include path: (system_lib_string) @name) @import
(preproc_include path: (string_literal) @name) @import
"#;

const CPP_SPANS: &str = r#"
(comment) @comment
(string_literal) @string
(char_literal) @string
(raw_string_literal) @string
"#;

/// C#.
const CSHARP_QUERY: &str = r#"
(class_declaration name: (identifier) @name body: (_) @body) @class
(interface_declaration name: (identifier) @name body: (_) @body) @interface
(struct_declaration name: (identifier) @name body: (_) @body) @struct
(enum_declaration name: (identifier) @name body: (_) @body) @enum
(record_declaration name: (identifier) @name) @record
(method_declaration name: (identifier) @name body: (block) @body) @method
(constructor_declaration name: (identifier) @name body: (block) @body) @method
(namespace_declaration name: (_) @name body: (_) @body) @namespace
(using_directive (identifier) @name) @import
(using_directive (qualified_name) @name) @import
"#;

const CSHARP_SPANS: &str = r#"
(comment) @comment
(string_literal) @string
(character_literal) @string
"#;

/// PHP (the `<?php` variant).
const PHP_QUERY: &str = r#"
(function_definition name: (name) @name body: (compound_statement) @body) @function
(method_declaration name: (name) @name body: (compound_statement) @body) @method
(class_declaration name: (name) @name body: (declaration_list) @body) @class
(interface_declaration name: (name) @name body: (declaration_list) @body) @interface
(trait_declaration name: (name) @name body: (declaration_list) @body) @trait
(enum_declaration name: (name) @name) @enum
(namespace_use_clause (qualified_name) @name) @import
; File inclusion (require/include and their _once variants) with a static string
; path — PHP's other import mechanism. Both single-quoted (string) and
; double-quoted (encapsed_string) forms; quotes stripped like other imports.
(require_expression [(string) (encapsed_string)] @name) @import
(require_once_expression [(string) (encapsed_string)] @name) @import
(include_expression [(string) (encapsed_string)] @name) @import
(include_once_expression [(string) (encapsed_string)] @name) @import
"#;

const PHP_SPANS: &str = r#"
(comment) @comment
(string) @string
(encapsed_string) @string
"#;

/// Syntactic class of a byte position, for `search --in`. Extracted at index
/// time and stored in the `spans` table, so `search --in` is a lookup, not a
/// re-parse.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Comment,
    Str,
}

impl Span {
    pub fn as_i64(self) -> i64 {
        match self {
            Span::Comment => 0,
            Span::Str => 1,
        }
    }

    pub fn from_i64(n: i64) -> Option<Span> {
        match n {
            0 => Some(Span::Comment),
            1 => Some(Span::Str),
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

const JS_SPANS: &str = r#"
(comment) @comment
(string) @string
(template_string) @string
"#;
