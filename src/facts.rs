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

use std::collections::HashMap;

use anyhow::{Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Point, Query, QueryCursor, Range};

mod docs;
use docs::{jinja_facts, markdown_facts, rst_facts};

mod queries;
use queries::{grammar, Grammar};

mod shape;
use shape::shape_of;

mod hcl;
use hcl::hcl_facts;

mod sql;
use sql::{
    repair_sql_identifier_escapes, sql_extras, sql_statement_ranges, unqualify,
    unquote_sql_identifier,
};

mod support;
pub use support::{has_spans, support};

mod liveness;
use liveness::RUNTIME_READ_NAMES;
pub use liveness::{framework_registration, unreferenced_by_design};

mod component;
use component::extract_component;
#[cfg(test)]
pub(crate) use component::{blank_outside, script_regions};

mod attrs;
use attrs::{registering_decorator, test_attributed};

mod signature;
use signature::{
    cap, collapse_ws, doc_of, signature_of, with_value_shape, wrapper_head, MAX_DOC, MAX_SIG,
};

/// A single extracted definition, before it is assigned a content-addressed id
/// (which needs the path, known only at store time).
pub struct Fact {
    pub kind: String,
    /// A supporting fact synthesized from a source declaration rather than a
    /// verbatim source-level definition. Qualified named imports use this to
    /// retain their exported name for resolution without looking like another
    /// import statement to user-facing queries.
    pub is_derived: bool,
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
    /// Normalized visibility: `public` (module/external API), `private`
    /// (module- or file-local), `restricted` (`pub(crate)`, `protected`). `None`
    /// for facts where it doesn't apply (imports) or can't be determined.
    pub visibility: Option<String>,
    /// The type a member belongs to — the struct/class/enum enclosing a `field`
    /// or `variant`, or the impl type of a Rust method. Members share names constantly
    /// (`id`, `name`, `path`), so the owner is what makes one addressable:
    /// without it `definitions retries` cannot say *which* type's field it found.
    pub owner: Option<String>,
    /// Hash of this definition's syntax *shape* — the tree of node kinds, with
    /// every identifier and literal normalised away — paired with the number of
    /// nodes hashed. Two definitions with the same shape are structurally
    /// identical up to renaming: a fact, not a similarity score, so there is no
    /// threshold to tune and no ranking to distrust. The node count exists so a
    /// caller can insist on substance; every empty function in a repository
    /// shares a shape, and truthfully so.
    pub shape: Option<String>,
    pub shape_nodes: i64,
    /// The definition is marked as a test by an attribute the grammar already
    /// parses — `#[test]`, `@pytest.mark.…`, `@Test`, `[Fact]`.
    ///
    /// Test recognition was a *label* check: the file path, the definition's own
    /// name, or the name of the module around it. So a `#[test] fn
    /// parses_an_empty_string` two lines below the function it tests was invisible
    /// to `codeq tests`, and every framework that does not make you name your
    /// tests `test_*` was unsupported by accident. The attribute is a fact sitting
    /// in the parse tree; this reads it instead of guessing from spelling.
    pub is_test: bool,
    /// A decorator that may register this definition with something codeq cannot
    /// see (`@app.get("/x")`, `@click.command()`). See [`registering_decorator`].
    pub decorator: Option<String>,
    /// The trait this definition exists to satisfy, if any.
    ///
    /// A Rust `impl Trait for X` method cannot carry `pub`, is invoked through
    /// the trait rather than by name, and cannot be deleted without breaking the
    /// build — so it is never dead code, however unreferenced it looks. Only the
    /// exact case is recorded: every method in a trait impl block is a trait
    /// method, which is a fact about the block, not a guess about the method.
    pub contract: Option<String>,
}

/// A type relation: `subtype` implements/extends `supertype`. Captures
/// `impl Trait for X` (Rust), `class X extends Y` / `implements I` (Java/TS/…),
/// `class X(Base)` (Python), etc. — the inheritance/implementation edges the
/// call graph doesn't cover.
pub struct Relation {
    /// The implementing/deriving type (`X` in `impl T for X`).
    pub subtype: String,
    /// The trait/interface/base being implemented or extended (`T`).
    pub supertype: String,
    /// `"implements"` or `"extends"`.
    pub relation: &'static str,
    pub start_line: usize,
}

impl Fact {
    /// Content-addressed id, stable within an index version + working-tree
    /// state. Encodes position, so it is deliberately *not* durable across
    /// edits — see the fact-model note in DESIGN.md.
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
/// What one parse of a file yields: its definitions, its comment/string spans,
/// and its type relations.
pub type Extraction = (Vec<Fact>, Vec<(usize, usize, Span)>, Vec<Relation>);

/// Parser recovery observed while producing an extraction. `errors` counts
/// Tree-sitter `ERROR` nodes in the tree actually harvested; `recovered` means
/// codeq selected a byte-preserving repaired parse over the grammar's first
/// recovery attempt.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParseHealth {
    pub errors: usize,
    pub recovered: bool,
}

/// Facts, parser health, and call sites. Languages extracted without a parser (Markdown)
/// and unsupported languages report `None`, rather than pretending they parsed
/// cleanly.
pub type Extracted = (
    Extraction,
    Option<ParseHealth>,
    Vec<crate::callsite::CallSite>,
);

#[derive(Default)]
pub struct Extractor {
    languages: HashMap<String, LangSpec>,
}

/// A SQL script parsed statement by statement: facts, spans and relations as
/// `harvest` returns them, then the ERROR node count and the bytes inside them.
type SqlStatements = (
    Vec<Fact>,
    Vec<(usize, usize, Span)>,
    Vec<Relation>,
    usize,
    usize,
);

struct LangSpec {
    parser: Parser,
    query: Query,
    capture_names: Vec<String>,
    span_query: Query,
    span_capture_names: Vec<String>,
    rel_query: Query,
    rel_capture_names: Vec<String>,
    source: String,
    lang: &'static str,
}

impl LangSpec {
    fn new(grammar: Grammar) -> Result<LangSpec> {
        let Grammar {
            name,
            language,
            defs,
            spans,
            rel,
        } = grammar;
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("setting tree-sitter language")?;
        let query = Query::new(&language, &defs).context("compiling query")?;
        let capture_names = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let span_query = Query::new(&language, spans).context("compiling span query")?;
        let span_capture_names = span_query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rel_query = Query::new(&language, rel).context("compiling relation query")?;
        let rel_capture_names = rel_query
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
            rel_query,
            rel_capture_names,
            source: format!("treesitter/{name}"),
            lang: name,
        })
    }

    fn extract(&mut self, src: &[u8]) -> Extracted {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return ((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()),
        };
        let original_calls = if crate::callsite::supports_syntax_calls(self.lang) {
            crate::callsite::call_sites_from_tree(self.lang, tree.root_node(), src)
        } else {
            Vec::new()
        };
        // tree-sitter-rust 0.24.2 has a recovery bug for a valid closure with
        // nested generic parameter types and a trailing parameter comma. It
        // wraps the rest of the file in ERROR, so the normal declaration query
        // cannot see any following items. Substitute just that comma with a
        // space (the same byte width), retry, and accept the tree only when it
        // has fewer error nodes. Coordinates and all source slices still refer
        // to the original bytes, so facts remain exact.
        if self.lang == "rust" && tree.root_node().has_error() {
            if let Some(repaired) = repair_rust_closure_trailing_commas(src) {
                if let Some(repaired_tree) = self.parser.parse(&repaired, None) {
                    if error_nodes(repaired_tree.root_node()) < error_nodes(tree.root_node()) {
                        let root = repaired_tree.root_node();
                        return (
                            self.harvest(root, src),
                            Some(ParseHealth {
                                recovered: true,
                                ..parse_health(root)
                            }),
                            original_calls,
                        );
                    }
                }
            }
        }
        let root = tree.root_node();
        (
            self.harvest(root, src),
            Some(parse_health(root)),
            original_calls,
        )
    }

    /// Run the three queries over an already-parsed tree. Split out of `extract`
    /// so an embedding language (Svelte) can walk the same root for its own
    /// purposes without paying for a second parse.
    fn harvest(&self, root: Node, src: &[u8]) -> Extraction {
        let mut facts = self.definitions(root, src);
        dedupe_members(&mut facts);
        (facts, self.spans(root, src), self.relations(root, src))
    }

    /// First pass: definitions and imports, one fact per query match.
    fn definitions(&self, root: Node, src: &[u8]) -> Vec<Fact> {
        let mut facts = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&self.query, root, src);
        while let Some(m) = it.next() {
            let mut name: Option<String> = None;
            let mut kind: Option<&str> = None;
            let mut def: Option<Node> = None;
            let mut body: Option<Node> = None;
            let mut guard: Option<&str> = None;
            // The value bound by a JS/TS module-scope `@constant`. Decides
            // whether this match duplicates a function captured elsewhere,
            // and where the signature should stop.
            let mut val: Option<Node> = None;
            // For `from MODULE import a, b` (Python): the module and each
            // imported name, combined below into qualified `MODULE.a` imports.
            let mut from_module: Option<&str> = None;
            let mut from_names: Vec<Node> = Vec::new();
            // The local spelling of an imported name (`make as m`). Import
            // rows retain the source module/name for dependency resolution;
            // this records what call sites actually write.
            let mut binding: Option<String> = None;
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
                    "val" => val = Some(cap.node),
                    "from_module" => from_module = cap.node.utf8_text(src).ok(),
                    "from_name" => from_names.push(cap.node),
                    "binding" => binding = cap.node.utf8_text(src).ok().map(str::to_string),
                    other => {
                        kind = Some(other);
                        def = Some(cap.node);
                    }
                }
            }
            // `from MODULE import a, b [as c]` → a qualified `MODULE.a` import
            // per imported name. The bare module string alone drops the name,
            // so the dependency graph can't resolve it to the file that defines
            // it; the qualified form stem-matches that file. Aliases keep the
            // original name (what determines the source file).
            if let Some(module) = from_module {
                for nn in &from_names {
                    if let Ok(local) = nn.utf8_text(src) {
                        let start = nn.start_position();
                        let end = nn.end_position();
                        facts.push(Fact {
                            kind: "import".to_string(),
                            is_derived: true,
                            name: format!("{module}.{local}"),
                            start_byte: nn.start_byte(),
                            end_byte: nn.end_byte(),
                            start_line: start.row + 1,
                            start_col: start.column,
                            end_line: end.row + 1,
                            end_col: end.column,
                            signature: binding.clone().unwrap_or_default(),
                            doc: None,
                            source: self.source.clone(),
                            confidence: 1.0,
                            visibility: None,
                            owner: None,
                            shape: None,
                            shape_nodes: 0,
                            is_test: false,
                            decorator: None,
                            contract: None,
                        });
                    }
                }
            }
            if let Some(g) = guard {
                if !guard_allows(self.lang, kind == Some("import"), g) {
                    continue;
                }
            }
            if let (Some(name), Some(kind), Some(node)) = (name, kind, def) {
                // `const f = () => …` matches both the `@function` pattern and
                // the module-scope `@constant` one. The function capture is
                // strictly better (it knows the body, so the signature folds),
                // so drop the constant rather than emit the binding twice.
                if kind == "constant"
                    && val.is_some_and(|v| {
                        matches!(v.kind(), "arrow_function" | "function_expression")
                            || is_require_binding(v, src)
                    })
                {
                    continue;
                }
                // A multi-line value — a wrapped component, a big object
                // literal — would collapse into an unreadable one-line
                // signature, so end the signature where it begins, the way a
                // function body does. A single-line value is kept whole:
                // `const VERSION = '1.0'` is more useful with its value.
                //
                // For a wrapped value, stop at the arguments rather than at
                // the value, so the wrapper survives: `const Card = memo(`
                // says what `const Card =` does not.
                let body = match val {
                    Some(v)
                        if kind == "constant" && v.end_position().row > v.start_position().row =>
                    {
                        Some(wrapper_head(v).unwrap_or(v))
                    }
                    _ => body,
                };
                let name = declared_name(self.lang, kind, name);
                // A function declared inside a type body is a method: it
                // can be called on a value. Several grammars use the same
                // node for both (Rust `function_item`, Python
                // `function_definition`), so the queries cannot tell them
                // apart and every method came back as `function`.
                let kind = if kind == "function" && is_method(self.lang, node) {
                    "method"
                } else {
                    kind
                };
                let visibility = effective_visibility(self.lang, kind, &name, node, src);
                let start = node.start_position();
                let end = node.end_position();
                // A member with no declaring type is not addressable — an
                // anonymous C struct's field, or a `self.x =` in a plain
                // function — and an owner-less `field` is exactly the bare
                // name this fact kind exists to avoid.
                let owner = owner_of(self.lang, kind, node, src);
                let (shape, shape_nodes) = shape_of(node);
                if matches!(kind, "field" | "variant") && owner.is_none() {
                    continue;
                }
                let (start_byte, start_line, start_col, signature) = if self.lang == "sql"
                    && kind == "field"
                    && node.start_byte() > 0
                    && src[node.start_byte() - 1] == b'['
                {
                    // The SQL grammar treats T-SQL's opening `[` as an
                    // external token on a column name, while the closing `]`
                    // remains in the column_definition range. Restore the
                    // source span so the signature is a faithful slice.
                    (
                        node.start_byte() - 1,
                        start.row + 1,
                        start.column.saturating_sub(1),
                        format!(
                            "[{}",
                            with_value_shape(signature_of(src, node, body), kind, val)
                        ),
                    )
                } else {
                    (
                        node.start_byte(),
                        start.row + 1,
                        start.column,
                        with_value_shape(signature_of(src, node, body), kind, val),
                    )
                };
                facts.push(Fact {
                    kind: kind.to_string(),
                    is_derived: false,
                    name,
                    start_byte,
                    end_byte: node.end_byte(),
                    start_line,
                    start_col,
                    end_line: end.row + 1,
                    end_col: end.column,
                    signature: if kind == "import" {
                        binding.unwrap_or_default()
                    } else {
                        signature
                    },
                    doc: doc_of(src, self.lang, node, body),
                    source: self.source.clone(),
                    confidence: 1.0,
                    visibility,
                    owner,
                    shape,
                    shape_nodes,
                    is_test: test_attributed(self.lang, node, src),
                    decorator: registering_decorator(node, src),
                    contract: implemented_trait(self.lang, node, src),
                });
            }
        }
        facts
    }

    /// Second pass: comment and string spans, ordered by start byte.
    fn spans(&self, root: Node, src: &[u8]) -> Vec<(usize, usize, Span)> {
        let mut spans = Vec::new();
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
        spans
    }

    /// Third pass: type relations (implements / extends). One match binds the
    /// subtype (`@sub`) once and one or more supertypes (`@implements` /
    /// `@extends`), so a class implementing several interfaces yields several.
    fn relations(&self, root: Node, src: &[u8]) -> Vec<Relation> {
        let mut relations = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&self.rel_query, root, src);
        while let Some(m) = it.next() {
            let mut sub: Option<&str> = None;
            let mut supers: Vec<(&str, &str)> = Vec::new(); // (relation, name)
            for cap in m.captures {
                let cname = self.rel_capture_names[cap.index as usize].as_str();
                let text = match cap.node.utf8_text(src) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                match cname {
                    "sub" => sub = Some(text),
                    "implements" => supers.push(("implements", text)),
                    "extends" => supers.push(("extends", text)),
                    // Not inheritance: a SQL foreign key, kept as its own
                    // kind so `--relation` can ask for one without the other.
                    "references" => supers.push(("references", text)),
                    _ => {}
                }
            }
            if let Some(sub) = sub {
                for (relation, sup) in supers {
                    relations.push(Relation {
                        subtype: sub.to_string(),
                        supertype: sup.to_string(),
                        relation,
                        start_line: m
                            .captures
                            .iter()
                            .map(|c| c.node.start_position().row + 1)
                            .min()
                            .unwrap_or(0),
                    });
                }
            }
        }
        relations
    }
}

/// A guarded match is only valid when the token is a known
/// keyword. Elixir definitions are macro calls (`def`/`defp`/
/// `defmodule`); every other language guards call-style imports.
/// The allow-list depends on what the match claims to be, not
/// just the language: in Elixir `defmodule M` and `alias M`
/// are the same shape of call, and only the keyword says
/// whether it defines a module or imports one. Checking the
/// keyword against the *kind* keeps them apart.
fn guard_allows(lang: &str, is_import: bool, g: &str) -> bool {
    match (lang, is_import) {
        ("elixir", false) => matches!(
            g,
            "def"
                | "defp"
                | "defmacro"
                | "defmacrop"
                | "defmodule"
                | "defprotocol"
                | "defimpl"
                | "defguard"
        ),
        ("elixir", true) => matches!(g, "alias" | "import" | "use" | "require"),
        // Python declares an instance attribute by assigning it,
        // and only a `self.`-qualified assignment declares one:
        // `other.x = 1` mutates somebody else's object. The member
        // query captures the receiver as `@guard` for this check,
        // so dropping this arm silently loses every Python
        // instance attribute.
        ("python", false) => g == "self",
        ("bash", _) => matches!(g, "source" | "."),
        ("lua", _) => matches!(g, "require" | "dofile" | "loadfile"),
        ("zig", _) => matches!(g, "@import" | "@cImport"),
        // Ruby uses method calls for both requires and field
        // declarations; the callee name says which.
        ("ruby", true) => matches!(
            g,
            "require" | "require_relative" | "require_dependency" | "load" | "autoload"
        ),
        ("ruby", false) => {
            matches!(g, "attr_accessor" | "attr_reader" | "attr_writer")
        }
        _ => matches!(
            g,
            "require" | "require_relative" | "require_dependency" | "load" | "autoload"
        ),
    }
}

/// The name a definition is known by, with the syntax its grammar captured
/// around it removed.
fn declared_name(lang: &str, kind: &str, name: String) -> String {
    // Some grammars capture the whole quoted import path (Go);
    // normalise to the bare module string like the others.
    let name = if kind == "import" {
        name.trim_matches(|c: char| "\"'`<>".contains(c))
            .to_string()
    } else if lang == "sql" && kind == "field" {
        unquote_sql_identifier(&name)
    } else if kind == "field" {
        // A Ruby `attr_accessor :a` declares the field `a`; the
        // colon is symbol syntax, not part of the name.
        name.trim_start_matches(':').to_string()
    } else {
        name
    };
    normalize_identifier(lang, &name)
}

/// A definition's visibility once the type around it is taken into account.
/// `None` for imports, where it does not apply.
fn effective_visibility(
    lang: &str,
    kind: &str,
    name: &str,
    node: Node,
    src: &[u8],
) -> Option<String> {
    if kind == "import" {
        return None;
    }
    if lang == "python" {
        let mut scope = node;
        // Instance fields belong to their class even when initialized inside
        // a method. Check whether that class is local, rather than the assignment.
        if kind == "field" {
            while let Some(parent) = scope.parent() {
                scope = parent;
                if scope.kind() == "class_definition" {
                    break;
                }
            }
        }
        if has_ancestor_kind(scope, "function_definition") {
            return Some("private".to_string());
        }
    }
    let own = visibility_of(lang, name, node, src);
    // A variant carries no visibility of its own in any
    // language codeq supports — Rust, Java, C# and TS all
    // forbid a modifier there, because `E::A` is reachable
    // exactly where `E` is. Asking the node made every one
    // of them `private`, so `api` reported a public enum's
    // surface as the bare enum name and `unused` labelled
    // its variants private. Inherit from the declaring type.
    let own = if kind == "variant" {
        enclosing_type_visibility(lang, node, src).or(own)
    } else {
        own
    };
    // A member of a private type is not public API in any
    // language codeq supports. Visibility was computed per
    // definition without consulting the type around it, so
    // the methods and fields of a Python `class _Draw` were
    // reported as public — putting them in `api`, in
    // `untested`, and into `api --compare-to` as newly *added*
    // public surface. The declaration is public; what it is
    // declared inside is what makes it reachable.
    let mut own = match own.as_deref() {
        Some("public") if enclosed_by_private_type(lang, node, src) => Some("private".to_string()),
        _ => own,
    };
    if lang == "rust" && own.as_deref() != Some("private") {
        let mut cur = node.parent();
        while let Some(parent) = cur {
            if parent.kind() == "mod_item" {
                match first_child_kind(parent, "visibility_modifier", src) {
                    None => return Some("private".to_string()),
                    Some("pub") => {}
                    Some(_) => own = Some("restricted".to_string()),
                }
            }
            cur = parent.parent();
        }
    }
    own
}

/// A member can be declared more than once — Python assigns `self.x` in
/// whichever methods touch it first — but it is still one member. Keep the
/// earliest declaration per (name, owner) so `definitions` shows a single
/// place rather than implying several. Names cannot repeat within one type
/// in any of these languages, so this cannot merge distinct members.
fn dedupe_members(facts: &mut Vec<Fact>) {
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    let mut keep: Vec<bool> = Vec::with_capacity(facts.len());
    let mut order: Vec<usize> = (0..facts.len()).collect();
    order.sort_by_key(|&i| facts[i].start_byte);
    let mut drop: std::collections::HashSet<usize> = Default::default();
    for &i in &order {
        let f = &facts[i];
        if !matches!(f.kind.as_str(), "field" | "variant") {
            continue;
        }
        let key = (f.name.clone(), f.owner.clone().unwrap_or_default());
        if !seen.insert(key) {
            drop.insert(i);
        }
    }
    for i in 0..facts.len() {
        keep.push(!drop.contains(&i));
    }
    let mut it = keep.iter();
    facts.retain(|_| *it.next().unwrap_or(&true));
}

/// Number of explicit parser recovery nodes in a tree. `has_error` alone is
/// not enough to choose between two parses: both may have errors, while one
/// still preserves substantially more of the file's structure.
fn error_nodes(root: Node) -> usize {
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            count += 1;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    count
}

/// How many source bytes sit inside an ERROR node, outermost nodes only. The
/// count of error nodes is a poor yardstick between two parses of one file: a
/// parser that gives up on one statement and wraps the rest of the file in a
/// single ERROR reports *fewer* errors than one that fails three small
/// statements, while understanding far less.
fn error_extent(root: Node) -> usize {
    let mut bytes = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            bytes += node.end_byte() - node.start_byte();
            continue;
        }
        if node.has_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    bytes
}

/// `has_error` is a cached tree property, so clean files avoid a second full
/// tree walk. Count individual recovery nodes only when the parser reports one.
fn parse_health(root: Node) -> ParseHealth {
    ParseHealth {
        errors: if root.has_error() {
            error_nodes(root)
        } else {
            0
        },
        recovered: false,
    }
}

/// Work around a Tree-sitter Rust recovery bug without changing any source
/// coordinate. The only replacement is a closure's trailing parameter comma:
/// `..., | -> Return { ... }` becomes `...  | -> Return { ... }`. The explicit
/// return arrow makes this distinct from an argument separator before a closure
/// expression (`call(value, |x| x)`), so valid source is never reinterpreted.
fn repair_rust_closure_trailing_commas(src: &[u8]) -> Option<Vec<u8>> {
    let mut repaired: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < src.len() {
        if src[i] != b',' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < src.len() && src[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= src.len() || src[j] != b'|' {
            i += 1;
            continue;
        }
        j += 1;
        while j < src.len() && src[j].is_ascii_whitespace() {
            j += 1;
        }
        if src.get(j..j + 2) == Some(b"->") {
            repaired.get_or_insert_with(|| src.to_vec())[i] = b' ';
        }
        i += 1;
    }
    repaired
}

impl LangSpec {
    /// Parse a SQL script one statement at a time. Each statement is parsed with
    /// the parser restricted to its own byte range, so nodes keep their offsets in
    /// the whole file and an unparseable statement stays an ERROR of its own
    /// instead of swallowing its successors. Returns the error node count and the
    /// bytes inside errors; `None` if a parse fails outright.
    fn extract_sql_by_statement(&mut self, parse_src: &[u8], src: &[u8]) -> Option<SqlStatements> {
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(
                parse_src
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b'\n')
                    .map(|(i, _)| i + 1),
            )
            .collect();
        let point = |byte: usize| {
            let row = line_starts.partition_point(|&l| l <= byte) - 1;
            Point::new(row, byte - line_starts[row])
        };
        let mut facts = Vec::new();
        let mut spans = Vec::new();
        let mut rels = Vec::new();
        let mut errors = 0;
        let mut extent = 0;
        let mut complete = true;
        for (start, end) in sql_statement_ranges(parse_src) {
            let range = Range {
                start_byte: start,
                end_byte: end,
                start_point: point(start),
                end_point: point(end),
            };
            if self.parser.set_included_ranges(&[range]).is_err() {
                complete = false;
                break;
            }
            let Some(tree) = self.parser.parse(parse_src, None) else {
                complete = false;
                break;
            };
            let root = tree.root_node();
            let (f, sp, r) = self.harvest(root, src);
            let (extra_facts, extra_spans) = sql_extras(root, src);
            facts.extend(f);
            facts.extend(extra_facts);
            spans.extend(sp);
            spans.extend(extra_spans);
            rels.extend(r);
            errors += parse_health(root).errors;
            extent += error_extent(root);
        }
        // The parser is shared across files: leave it parsing the whole input.
        let _ = self.parser.set_included_ranges(&[]);
        complete.then_some((facts, spans, rels, errors, extent))
    }
}

impl Extractor {
    fn load(&mut self, language: &str) -> Result<()> {
        if self.languages.contains_key(language) {
            return Ok(());
        }
        let Some(grammar) = grammar(language) else {
            return Ok(());
        };
        let spec = LangSpec::new(grammar)?;
        self.languages.insert(language.to_string(), spec);
        Ok(())
    }

    /// Compile each language's queries on first use, then reuse its parser.
    /// Query errors propagate to the caller instead of publishing an empty index.
    pub fn extract(&mut self, language: &str, path: &str, src: &[u8]) -> Result<Extracted> {
        match language {
            "markdown" => return Ok((markdown_facts(src), None, Vec::new())),
            "rst" => return Ok((rst_facts(src), None, Vec::new())),
            "jinja" => return Ok((jinja_facts(src), None, Vec::new())),
            _ => {}
        }
        self.load(language)?;
        if matches!(language, "svelte" | "vue") {
            self.load("javascript")?;
            self.load("typescript")?;
            let [Some(markup), Some(javascript), Some(typescript)] = self
                .languages
                .get_disjoint_mut([language, "javascript", "typescript"])
            else {
                unreachable!("component parsers were just loaded");
            };
            return Ok(extract_component(markup, javascript, typescript, path, src));
        }
        let Some(spec) = self.languages.get_mut(language) else {
            return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new()));
        };
        Ok(match language {
            "hcl" => {
                let tree = match spec.parser.parse(src, None) {
                    Some(t) => t,
                    None => return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new())),
                };
                let root = tree.root_node();
                let (mut facts, spans, rels) = spec.harvest(root, src);
                facts.extend(hcl_facts(root, src));
                ((facts, spans, rels), Some(parse_health(root)), Vec::new())
            }
            "sql" => {
                // tree-sitter-sequel does not accept doubled delimiters inside
                // quoted identifiers and does not parse T-SQL bracket quoting.
                // Substitute only escape syntax with same-width identifier bytes
                // so node offsets remain identical, then harvest text/ranges from
                // the untouched source below.
                let repaired = repair_sql_identifier_escapes(src);
                let parse_src = repaired.as_deref().unwrap_or(src);
                let tree = match spec.parser.parse(parse_src, None) {
                    Some(t) => t,
                    None => return Ok(((Vec::new(), Vec::new(), Vec::new()), None, Vec::new())),
                };
                let root = tree.root_node();
                let original_calls = if repaired.is_some() {
                    spec.parser
                        .parse(src, None)
                        .map(|original| {
                            crate::callsite::call_sites_from_tree(
                                language,
                                original.root_node(),
                                src,
                            )
                        })
                        .unwrap_or_default()
                } else {
                    crate::callsite::call_sites_from_tree(language, root, src)
                };
                let (mut facts, mut spans, rels, health) = {
                    // A statement the grammar does not know (`CREATE OPERATOR`,
                    // `CREATE CAST`, a psql `\echo`) is recovered from by
                    // wrapping *everything after it* in one ERROR, so on a
                    // PostgreSQL extension script the first one costs every
                    // definition below it. Parse each statement on its own
                    // instead, and keep that only when the parser understood
                    // more of the file.
                    let whole_extent = error_extent(root);
                    let isolated = root
                        .has_error()
                        .then(|| spec.extract_sql_by_statement(parse_src, src))
                        .flatten()
                        .filter(|(.., extent)| *extent < whole_extent);
                    match isolated {
                        Some((facts, spans, rels, errors, _)) => (
                            facts,
                            spans,
                            rels,
                            ParseHealth {
                                errors,
                                recovered: true,
                            },
                        ),
                        None => {
                            let (facts, spans, rels) = spec.harvest(root, src);
                            let (extra_facts, extra_spans) = sql_extras(root, src);
                            let mut facts = facts;
                            let mut spans = spans;
                            facts.extend(extra_facts);
                            spans.extend(extra_spans);
                            (facts, spans, rels, parse_health(root))
                        }
                    }
                };
                spans.sort_by_key(|(s, _, _)| *s);
                for f in &mut facts {
                    f.name = unqualify(&f.name);
                    f.owner = f.owner.as_deref().map(unqualify);
                }
                ((facts, spans, rels), Some(health), original_calls)
            }
            _ => spec.extract(src),
        })
    }
}

fn is_method(lang: &str, node: Node) -> bool {
    if lang == "rust" && !has_self_parameter(node) {
        return false;
    }
    let mut cur = node.parent();
    while let Some(n) = cur {
        let k = n.kind();
        // A nearer function encloses this one: nested, not a member.
        if k.contains("function_item")
            || k.contains("function_definition")
            || k.contains("function_declaration")
            || k.contains("method")
        {
            return false;
        }
        if is_type_body(k) {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// Node kinds that introduce a type's body across the supported grammars.
fn is_type_body(kind: &str) -> bool {
    const TYPEISH: &[&str] = &[
        "impl",
        "trait",
        "class",
        "struct",
        "contract",
        "object",
        "protocol",
        "extension",
        "interface",
        "enum",
        "record",
    ];
    TYPEISH.iter().any(|t| kind.contains(t))
}

/// A Rust `fn` whose first parameter is `self` (`&self`, `&mut self`, `self`).
fn has_self_parameter(node: Node) -> bool {
    let Some(params) = node.child_by_field_name("parameters") else {
        return false;
    };
    let mut c = params.walk();
    let found = params
        .named_children(&mut c)
        .any(|p| p.kind() == "self_parameter");
    found
}

/// Every language codeq extracts from.
///
/// This is what `capabilities` reports, so a language missing here reads as
/// unsupported even when it extracts perfectly well — which is how svelte, vue,
/// css, hcl, powershell, proto and graphql went seven releases undocumented. A
/// test pins it against `index::EXTENSIONS` so the omission cannot recur.
pub const LANGUAGES: &[&str] = &[
    "rust",
    "python",
    "javascript",
    "typescript",
    "tsx",
    "go",
    "java",
    "ruby",
    "c",
    "cpp",
    "csharp",
    "php",
    "bash",
    "lua",
    "scala",
    "kotlin",
    "swift",
    "dart",
    "zig",
    "elixir",
    "solidity",
    "svelte",
    "vue",
    "css",
    "hcl",
    "powershell",
    "proto",
    "graphql",
    "markdown",
    "rst",
    "jinja",
    "sql",
];

/// The text of the first direct child of `node` whose kind is `kind`.
fn first_child_kind<'a>(node: Node, kind: &str, src: &'a [u8]) -> Option<&'a str> {
    let mut c = node.walk();
    let found = node.children(&mut c).find(|n| n.kind() == kind);
    found.and_then(|n| n.utf8_text(src).ok())
}

/// Node kinds that declare a type a member can belong to. Deliberately a fixed
/// list rather than "anything with a name": a field's owner is the nearest
/// *type*, not the nearest function or block that happens to enclose it.
const TYPE_KINDS: &[&str] = &[
    // Rust
    "struct_item",
    "enum_item",
    "union_item",
    // Go
    "type_spec",
    // Java / C# / TS / JS / Python / PHP / Swift / Kotlin
    "class_declaration",
    "class_definition",
    "interface_declaration",
    "struct_declaration",
    "enum_declaration",
    "record_declaration",
    "object_declaration",
    "protocol_declaration",
    // Zig / Solidity / Dart / Scala
    "container_declaration",
    "struct_declaration",
    "contract_declaration",
    "library_declaration",
    "class_definition",
    "object_definition",
    "trait_definition",
    "enum_definition",
    // Protobuf / GraphQL
    "message",
    "enum",
    "service",
    "object_type_definition",
    "interface_type_definition",
    "input_object_type_definition",
    "enum_type_definition",
    // C / C++
    "struct_specifier",
    "class_specifier",
    "union_specifier",
    "enum_specifier",
    // Ruby. Its grammar names the nodes plainly, with no `_declaration` suffix —
    // which is how `attr_accessor :a` fields went missing when Ruby member
    // extraction met the rule below that an owner-less member is dropped. Each
    // side was right on its own; only together did they cancel out.
    "class",
    "module",
    "singleton_class",
    // SQL. `alter_table` is here alongside `create_table` because in a migrations
    // directory — the shape most repos actually have — the column is added by a
    // later `ALTER TABLE`, not by the original `CREATE`. Without it every such
    // column resolves to no owner and is dropped, so the newest half of a schema
    // would be the invisible half.
    "create_table",
    "alter_table",
    "create_view",
    "create_materialized_view",
    "create_type",
];

/// Whether `n` declares a type. Kind alone is not enough, because the list is
/// shared across grammars and one grammar's type is another's file: Ruby's
/// `module Foo` and the root of every Python file are both a `module` node. A
/// file is not a type, and asking one for its name scans all of its top-level
/// children. That happened once per definition, which made a Python file of N
/// definitions cost N² to index.
fn is_type_node(n: Node) -> bool {
    TYPE_KINDS.contains(&n.kind()) && n.parent().is_some()
}

/// The trait a definition exists to satisfy, when the grammar states it outright.
///
/// Rust only, and deliberately: `impl Trait for X { … }` says that *every*
/// method in the block implements `Trait`, which is a fact about the block
/// rather than a guess about the method. The Java/C#/TypeScript analogue —
/// a method of a class that `implements I` — is not the same claim, because
/// only some of those methods come from `I`, and answering which needs the
/// interface resolved.
fn implemented_trait(lang: &str, node: Node, src: &[u8]) -> Option<String> {
    if lang != "rust" {
        return None;
    }
    let mut cur = node.parent();
    while let Some(n) = cur {
        // A nested function inside a trait method does not implement anything.
        if n.kind() == "function_item" {
            return None;
        }
        if n.kind() == "impl_item" {
            let t = n.child_by_field_name("trait")?;
            return t.utf8_text(src).ok().map(collapse_ws);
        }
        cur = n.parent();
    }
    None
}

/// The visibility of the nearest type declaration enclosing `node`.
///
/// For members that cannot declare one themselves. Only the *nearest* type is
/// consulted: an outer private type still demotes the result, but through
/// `enclosed_by_private_type`, which already walks the whole chain.
fn enclosing_type_visibility(lang: &str, node: Node, src: &[u8]) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if is_type_node(n) {
            let name = type_name_of(lang, n, src)?;
            return visibility_of(lang, &name, n, src);
        }
        cur = n.parent();
    }
    None
}

/// The declared name of a type node, however its grammar spells the field.
pub(crate) fn normalize_identifier(lang: &str, name: &str) -> String {
    match lang {
        // Rust raw identifiers and C# verbatim identifiers escape a keyword;
        // the prefix is syntax and is not part of the declared identifier.
        "rust" => name.strip_prefix("r#").unwrap_or(name).to_string(),
        "csharp" => name.strip_prefix('@').unwrap_or(name).to_string(),
        _ => name.to_string(),
    }
}

fn type_name_of(lang: &str, n: Node, src: &[u8]) -> Option<String> {
    let mut c = n.walk();
    let named = n.children(&mut c).find(|ch| {
        let k = ch.kind();
        k.ends_with("identifier") || k == "name" || k.ends_with("_name")
    });
    n.child_by_field_name("name")
        .or(named)
        .and_then(|c| c.utf8_text(src).ok())
        .map(|name| normalize_identifier(lang, name))
}

/// Whether any type declaration enclosing `node` is itself private.
///
/// Walks every enclosing type, not just the nearest: a public inner class inside
/// a private outer one is still unreachable from outside, so the most restrictive
/// answer on the chain is the true one.
fn enclosed_by_private_type(lang: &str, node: Node, src: &[u8]) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if is_type_node(n) {
            if let Some(name) = type_name_of(lang, n, src) {
                if visibility_of(lang, &name, n, src).as_deref() == Some("private") {
                    return true;
                }
            }
        }
        cur = n.parent();
    }
    false
}

/// The name of the type enclosing a field, variant, or Rust impl method.
/// Walks up to the nearest type declaration and reads its name — the identifier
/// child, whatever the grammar happens to call it. `None` for non-members, and
/// for a member whose owner is anonymous (a bare `struct { … }` in C).
fn owner_of(lang: &str, kind: &str, node: Node, src: &[u8]) -> Option<String> {
    if lang == "rust" && kind == "function" {
        let mut cur = node.parent();
        while let Some(n) = cur {
            // A nested helper is not a method of the surrounding impl.
            if n.kind() == "function_item" {
                return None;
            }
            if n.kind() == "impl_item" {
                let mut ty = n.child_by_field_name("type")?;
                if ty.kind() == "generic_type" {
                    ty = ty.child_by_field_name("type")?;
                }
                return ty
                    .utf8_text(src)
                    .ok()
                    .map(|t| t.rsplit("::").next().unwrap_or(t).trim().to_string());
            }
            cur = n.parent();
        }
        return None;
    }
    if kind != "field" && kind != "variant" {
        return None;
    }
    let mut cur = node.parent();
    while let Some(n) = cur {
        if is_type_node(n) {
            // `name:` where the grammar has it; else the first identifier-ish
            // child, which is where the rest put it.
            if let Some(t) = n
                .child_by_field_name("name")
                .and_then(|c| c.utf8_text(src).ok())
            {
                return Some(normalize_identifier(lang, t));
            }
            // No `name:` field, so take the first child that *is* the name.
            // `_name` catches the grammars that spell it out per construct
            // (protobuf's `message_name`, `enum_name`, …).
            let mut c = n.walk();
            let own = n
                .children(&mut c)
                .find(|ch| {
                    let k = ch.kind();
                    // `object_reference` is how SQL spells "the thing being
                    // declared"; it wraps the identifier rather than being one.
                    k.ends_with("identifier")
                        || k == "name"
                        || k.ends_with("_name")
                        || k == "object_reference"
                })
                .and_then(|ch| ch.utf8_text(src).ok())
                .map(|t| normalize_identifier(lang, t));
            if own.is_some() {
                return own;
            }
            // Some languages bind a type to a name rather than declaring it with
            // one: Zig's `const Point = struct { … }` leaves the struct itself
            // anonymous and puts `Point` on the enclosing declaration. Look there
            // before giving up, or every field of every Zig type is ownerless and
            // silently dropped.
            let parent = n.parent()?;
            let mut pc = parent.walk();
            return parent
                .children(&mut pc)
                .find(|ch| ch.kind().ends_with("identifier"))
                .and_then(|ch| ch.utf8_text(src).ok())
                .map(|t| normalize_identifier(lang, t));
        }
        cur = n.parent();
    }
    None
}

/// Whether any ancestor of `node` has the given kind (walks up to the root).
fn has_ancestor_kind(node: Node, kind: &str) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == kind {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// The concatenated text of a definition's modifier node(s): Java has a single
/// `modifiers` wrapper, C# has repeated `modifier` children — handle both.
fn modifier_words(def: Node, src: &[u8]) -> String {
    let mut c = def.walk();
    let mut out = String::new();
    for ch in def.children(&mut c) {
        if ch.kind() == "modifiers" || ch.kind() == "modifier" {
            if let Ok(t) = ch.utf8_text(src) {
                out.push_str(t);
                out.push(' ');
            }
        }
    }
    out
}

/// Normalized visibility of a definition, by each language's own notion of it.
/// `public` = the external/module API; `private` = module- or file-local;
/// `restricted` = crate-visible (`pub(crate)`) or `protected`. Heuristic where a
/// language has no keyword: Go by name case, Python by leading underscore, and
/// Ruby is always reported public (its `private`/`protected` are stateful section
/// markers this doesn't track). `None` when it doesn't apply or is unknown.
fn visibility_of(lang: &str, name: &str, def: Node, src: &[u8]) -> Option<String> {
    let s = |v: &str| Some(v.to_string());
    match lang {
        "rust" => match first_child_kind(def, "visibility_modifier", src) {
            None => s("private"),
            Some("pub") => s("public"),
            Some(_) => s("restricted"), // pub(crate) / pub(super) / pub(in …)
        },
        "go" => match name.chars().next() {
            Some(c) if c.is_uppercase() => s("public"),
            Some(_) => s("private"),
            None => None,
        },
        "python" => {
            // A dunder is normally public — `__init__`, `__enter__` and friends
            // are how a type is used. But the module-level names the *interpreter*
            // reads are metadata, not surface: listing `__all__` as public API
            // says the module exports a name nobody can call.
            if RUNTIME_READ_NAMES
                .iter()
                .any(|(l, n)| *l == "python" && *n == name)
            {
                return s("private");
            }
            let dunder = name.starts_with("__") && name.ends_with("__");
            if name.starts_with('_') && !dunder {
                s("private")
            } else {
                s("public")
            }
        }
        "javascript" | "typescript" | "tsx" => {
            // Members carry their own modifier; only a *top-level* definition
            // takes its visibility from whether the file exports it. Without the
            // field kinds here, `private secret` in an exported class was
            // reported as public API purely because the class was exported.
            if matches!(
                def.kind(),
                "method_definition" | "public_field_definition" | "property_signature"
            ) {
                // Members are public by default; only a modifier or a #private
                // name restricts them.
                if name.starts_with('#') {
                    return s("private");
                }
                match first_child_kind(def, "accessibility_modifier", src) {
                    Some("private") => s("private"),
                    Some("protected") => s("restricted"),
                    _ => s("public"),
                }
            } else if has_ancestor_kind(def, "export_statement") {
                s("public")
            } else if def.kind() == "assignment_expression" {
                // `exports.f = …` / `module.exports.f = …` *is* the export in
                // CommonJS, so treating it like any other unexported top-level
                // definition would call every Node module's public API private.
                // Only the direct forms: whether some other object escapes
                // (express's `var proto = module.exports = …`) needs data flow
                // codeq does not do, so those stay private rather than guess.
                let exported = def
                    .child_by_field_name("left")
                    .and_then(|l| l.child_by_field_name("object"))
                    .and_then(|o| o.utf8_text(src).ok())
                    .is_some_and(|o| o == "exports" || o == "module.exports");
                if exported {
                    s("public")
                } else {
                    s("private")
                }
            } else {
                s("private") // a top-level definition that isn't exported
            }
        }
        "java" | "csharp" => {
            let mods = modifier_words(def, src);
            if mods.contains("public") {
                s("public")
            } else if mods.contains("protected") {
                s("restricted")
            } else {
                s("private") // no/other modifier = package-private / internal
            }
        }
        "php" => match first_child_kind(def, "visibility_modifier", src) {
            Some(m) if m.contains("public") => s("public"),
            Some(m) if m.contains("protected") => s("restricted"),
            Some(_) => s("private"),
            None => s("public"), // a top-level function is globally callable
        },
        "c" | "cpp" => {
            if first_child_kind(def, "storage_class_specifier", src) == Some("static") {
                s("private") // internal linkage
            } else {
                s("public")
            }
        }
        "ruby" => s("public"), // section-based private/protected not tracked
        // A schema has no private half: anything a connection can name, it can
        // use. Calling it public is what makes `api schema.sql` answer "what does
        // this schema expose" — and what puts a never-referenced table into
        // `unused` flagged as possibly-external rather than as certain dead code.
        "sql" => s("public"),
        _ => None,
    }
}

/// Whether a bound value is really an import: `require('x')`, `require('x').y`,
/// or `require('x')(…)`.
///
/// CommonJS writes every dependency as a module-scope binding, and codeq already
/// captures those as `@import` facts. Counting them again as definitions buries
/// each file's `outline` under its own import block — ten of the first twelve
/// rows in Express's `lib/utils.js` — and would have them turn up in `unused`
/// and `hotspots` as though they were declarations.
fn is_require_binding(val: Node, src: &[u8]) -> bool {
    // Walk down the leftmost spine: the callee of a call, the object of a member
    // access. Each step is a strict child, so this terminates.
    let mut n = val;
    loop {
        let next = match n.kind() {
            "call_expression" => {
                let f = n.child_by_field_name("function");
                if f.is_some_and(|f| f.kind() == "identifier" && f.utf8_text(src) == Ok("require"))
                {
                    return true;
                }
                f
            }
            "member_expression" => n.child_by_field_name("object"),
            _ => return false,
        };
        match next {
            Some(c) => n = c,
            None => return false,
        }
    }
}

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

#[cfg(test)]
mod extractor_tests {
    use super::Extractor;

    #[test]
    fn shared_tree_calls_match_standalone_parses_for_language_fixtures() {
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/languages");
        let mut extractor = Extractor::default();
        let mut checked = 0;
        for entry in std::fs::read_dir(fixtures).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().unwrap().to_str().unwrap();
            let language = crate::index::language_for(name).unwrap();
            let src = std::fs::read_to_string(&path).unwrap();
            let (_, _, actual) = extractor.extract(language, name, src.as_bytes()).unwrap();
            let expected = crate::callsite::call_sites_in(language, &src).unwrap_or_default();
            let rows = |sites: Vec<crate::callsite::CallSite>| {
                sites
                    .into_iter()
                    .map(|s| (s.start_byte, s.name, s.receiver))
                    .collect::<Vec<_>>()
            };
            assert_eq!(rows(actual), rows(expected), "{name}");
            checked += 1;
        }
        assert_eq!(checked, super::LANGUAGES.len());
    }

    #[test]
    fn fact_recovery_preserves_calls_from_the_original_tree() {
        let mut extractor = Extractor::default();
        for (language, src) in [
            ("rust", "fn before() { target(); }\nfn closure() { let f = |x: Option<Vec<String>>,| { target(); }; }\nfn after() { target(); }\n"),
            ("sql", "CREATE TABLE t (\"quote\"\"name\" TEXT); SELECT target();"),
            ("sql", "SELECT target(); CREATE OPERATOR === (FUNCTION = eq); SELECT target();"),
        ] {
            let (_, _, actual) = extractor.extract(language, "sample", src.as_bytes()).unwrap();
            let expected = crate::callsite::call_sites_in(language, src).unwrap();
            let rows = |sites: Vec<crate::callsite::CallSite>| {
                sites.into_iter().map(|s| (s.start_byte, s.name, s.receiver)).collect::<Vec<_>>()
            };
            assert!(!expected.is_empty(), "{language}: {src}");
            assert_eq!(rows(actual), rows(expected), "{language}: {src}");
        }
    }

    #[test]
    fn one_extractor_handles_alternating_languages_and_new_source() {
        let mut extractor = Extractor::default();
        for (language, source, expected) in [
            ("python", "def first():\n    pass\n", "first"),
            ("rust", "pub fn second() {}", "second"),
            ("python", "def third():\n    pass\n", "third"),
            ("rust", "pub fn fourth() {}", "fourth"),
        ] {
            let ((facts, _, _), health, _) = extractor
                .extract(language, "sample", source.as_bytes())
                .unwrap();
            let names: Vec<_> = facts.iter().map(|fact| fact.name.as_str()).collect();
            assert_eq!(names, [expected]);
            assert_eq!(health.unwrap().errors, 0);
        }
    }
}
