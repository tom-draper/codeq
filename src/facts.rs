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

mod queries;
use queries::*;

/// Cap signatures/docs so a pathological declaration can't blow the budget.
const MAX_SIG: usize = 200;
const MAX_DOC: usize = 240;

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
    /// or `variant`. `None` for everything else. Members share names constantly
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

/// Facts plus parser health. Languages extracted without a parser (Markdown)
/// and unsupported languages report `None`, rather than pretending they parsed
/// cleanly.
pub type Extracted = (Extraction, Option<ParseHealth>);

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
    bash: LangSpec,
    lua: LangSpec,
    scala: LangSpec,
    kotlin: LangSpec,
    swift: LangSpec,
    dart: LangSpec,
    zig: LangSpec,
    elixir: LangSpec,
    solidity: LangSpec,
    svelte: LangSpec,
    vue: LangSpec,
    css: LangSpec,
    hcl: LangSpec,
    powershell: LangSpec,
    proto: LangSpec,
    graphql: LangSpec,
    sql: LangSpec,
}

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
    fn new(
        language: tree_sitter::Language,
        query_src: &str,
        span_src: &str,
        rel_src: &str,
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
        let span_query = Query::new(&language, span_src).context("compiling span query")?;
        let span_capture_names = span_query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rel_query = Query::new(&language, rel_src).context("compiling relation query")?;
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
            source: source.to_string(),
            lang,
        })
    }

    /// One parse yields definition facts, comment/string spans, and type relations.
    fn extract(&mut self, src: &[u8]) -> Extracted {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return ((Vec::new(), Vec::new(), Vec::new()), None),
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
                        );
                    }
                }
            }
        }
        let root = tree.root_node();
        (self.harvest(root, src), Some(parse_health(root)))
    }

    /// Run the three queries over an already-parsed tree. Split out of `extract`
    /// so an embedding language (Svelte) can walk the same root for its own
    /// purposes without paying for a second parse.
    fn harvest(
        &self,
        root: Node,
        src: &[u8],
    ) -> (Vec<Fact>, Vec<(usize, usize, Span)>, Vec<Relation>) {
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
                    let is_import = kind == Some("import");
                    // A guarded match is only valid when the token is a known
                    // keyword. Elixir definitions are macro calls (`def`/`defp`/
                    // `defmodule`); every other language guards call-style imports.
                    // The allow-list depends on what the match claims to be, not
                    // just the language: in Elixir `defmodule M` and `alias M`
                    // are the same shape of call, and only the keyword says
                    // whether it defines a module or imports one. Checking the
                    // keyword against the *kind* keeps them apart.
                    let ok = match (self.lang, is_import) {
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
                            "require"
                                | "require_relative"
                                | "require_dependency"
                                | "load"
                                | "autoload"
                        ),
                        ("ruby", false) => {
                            matches!(g, "attr_accessor" | "attr_reader" | "attr_writer")
                        }
                        _ => matches!(
                            g,
                            "require"
                                | "require_relative"
                                | "require_dependency"
                                | "load"
                                | "autoload"
                        ),
                    };
                    if !ok {
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
                            if kind == "constant"
                                && v.end_position().row > v.start_position().row =>
                        {
                            Some(wrapper_head(v).unwrap_or(v))
                        }
                        _ => body,
                    };
                    // Some grammars capture the whole quoted import path (Go);
                    // normalise to the bare module string like the others.
                    let name = if kind == "import" {
                        name.trim_matches(|c: char| "\"'`<>".contains(c))
                            .to_string()
                    } else if self.lang == "sql" && kind == "field" {
                        unquote_sql_identifier(&name)
                    } else if kind == "field" {
                        // A Ruby `attr_accessor :a` declares the field `a`; the
                        // colon is symbol syntax, not part of the name.
                        name.trim_start_matches(':').to_string()
                    } else {
                        name
                    };
                    let name = normalize_identifier(self.lang, &name);
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
                    let visibility = if kind == "import" {
                        None
                    } else {
                        let own = visibility_of(self.lang, &name, node, src);
                        // A variant carries no visibility of its own in any
                        // language codeq supports — Rust, Java, C# and TS all
                        // forbid a modifier there, because `E::A` is reachable
                        // exactly where `E` is. Asking the node made every one
                        // of them `private`, so `api` reported a public enum's
                        // surface as the bare enum name and `unused` labelled
                        // its variants private. Inherit from the declaring type.
                        let own = if kind == "variant" {
                            enclosing_type_visibility(self.lang, node, src).or(own)
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
                        match own.as_deref() {
                            Some("public") if enclosed_by_private_type(self.lang, node, src) => {
                                Some("private".to_string())
                            }
                            _ => own,
                        }
                    };
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
        }

        // A member can be declared more than once — Python assigns `self.x` in
        // whichever methods touch it first — but it is still one member. Keep the
        // earliest declaration per (name, owner) so `definitions` shows a single
        // place rather than implying several. Names cannot repeat within one type
        // in any of these languages, so this cannot merge distinct members.
        {
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

        // Third pass: type relations (implements / extends). One match binds the
        // subtype (`@sub`) once and one or more supertypes (`@implements` /
        // `@extends`), so a class implementing several interfaces yields several.
        let mut relations = Vec::new();
        {
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
        }

        (facts, spans, relations)
    }
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

impl Extractor {
    pub fn new() -> Result<Extractor> {
        Ok(Extractor {
            rust: LangSpec::new(
                tree_sitter_rust::LANGUAGE.into(),
                RUST_QUERY,
                RUST_SPANS,
                RUST_REL,
                "treesitter/rust",
                "rust",
            )?,
            python: LangSpec::new(
                tree_sitter_python::LANGUAGE.into(),
                PYTHON_QUERY,
                PYTHON_SPANS,
                PYTHON_REL,
                "treesitter/python",
                "python",
            )?,
            javascript: LangSpec::new(
                tree_sitter_javascript::LANGUAGE.into(),
                &format!("{JS_QUERY}\n{JS_EXTRA}"),
                JS_SPANS,
                JS_REL,
                "treesitter/javascript",
                "javascript",
            )?,
            typescript: LangSpec::new(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                &format!("{JS_QUERY}\n{TS_EXTRA}"),
                JS_SPANS,
                TS_REL,
                "treesitter/typescript",
                "typescript",
            )?,
            tsx: LangSpec::new(
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                &format!("{JS_QUERY}\n{TS_EXTRA}"),
                JS_SPANS,
                TS_REL,
                "treesitter/tsx",
                "tsx",
            )?,
            go: LangSpec::new(
                tree_sitter_go::LANGUAGE.into(),
                GO_QUERY,
                GO_SPANS,
                GO_REL,
                "treesitter/go",
                "go",
            )?,
            java: LangSpec::new(
                tree_sitter_java::LANGUAGE.into(),
                JAVA_QUERY,
                JAVA_SPANS,
                JAVA_REL,
                "treesitter/java",
                "java",
            )?,
            ruby: LangSpec::new(
                tree_sitter_ruby::LANGUAGE.into(),
                RUBY_QUERY,
                RUBY_SPANS,
                RUBY_REL,
                "treesitter/ruby",
                "ruby",
            )?,
            c: LangSpec::new(
                tree_sitter_c::LANGUAGE.into(),
                C_QUERY,
                C_SPANS,
                C_REL,
                "treesitter/c",
                "c",
            )?,
            cpp: LangSpec::new(
                tree_sitter_cpp::LANGUAGE.into(),
                CPP_QUERY,
                CPP_SPANS,
                CPP_REL,
                "treesitter/cpp",
                "cpp",
            )?,
            csharp: LangSpec::new(
                tree_sitter_c_sharp::LANGUAGE.into(),
                CSHARP_QUERY,
                CSHARP_SPANS,
                CSHARP_REL,
                "treesitter/csharp",
                "csharp",
            )?,
            php: LangSpec::new(
                tree_sitter_php::LANGUAGE_PHP.into(),
                PHP_QUERY,
                PHP_SPANS,
                PHP_REL,
                "treesitter/php",
                "php",
            )?,
            bash: LangSpec::new(
                tree_sitter_bash::LANGUAGE.into(),
                BASH_QUERY,
                BASH_SPANS,
                "",
                "treesitter/bash",
                "bash",
            )?,
            lua: LangSpec::new(
                tree_sitter_lua::LANGUAGE.into(),
                LUA_QUERY,
                LUA_SPANS,
                "",
                "treesitter/lua",
                "lua",
            )?,
            scala: LangSpec::new(
                tree_sitter_scala::LANGUAGE.into(),
                SCALA_QUERY,
                SCALA_SPANS,
                SCALA_REL,
                "treesitter/scala",
                "scala",
            )?,
            kotlin: LangSpec::new(
                tree_sitter_kotlin_ng::LANGUAGE.into(),
                KOTLIN_QUERY,
                KOTLIN_SPANS,
                KOTLIN_REL,
                "treesitter/kotlin",
                "kotlin",
            )?,
            swift: LangSpec::new(
                tree_sitter_swift::LANGUAGE.into(),
                SWIFT_QUERY,
                SWIFT_SPANS,
                SWIFT_REL,
                "treesitter/swift",
                "swift",
            )?,
            dart: LangSpec::new(
                tree_sitter_dart::LANGUAGE.into(),
                DART_QUERY,
                DART_SPANS,
                DART_REL,
                "treesitter/dart",
                "dart",
            )?,
            zig: LangSpec::new(
                tree_sitter_zig::LANGUAGE.into(),
                ZIG_QUERY,
                ZIG_SPANS,
                "",
                "treesitter/zig",
                "zig",
            )?,
            elixir: LangSpec::new(
                tree_sitter_elixir::LANGUAGE.into(),
                ELIXIR_QUERY,
                ELIXIR_SPANS,
                "",
                "treesitter/elixir",
                "elixir",
            )?,
            solidity: LangSpec::new(
                tree_sitter_solidity::LANGUAGE.into(),
                SOLIDITY_QUERY,
                SOLIDITY_SPANS,
                SOLIDITY_REL,
                "treesitter/solidity",
                "solidity",
            )?,
            svelte: LangSpec::new(
                tree_sitter_svelte_ng::LANGUAGE.into(),
                SVELTE_QUERY,
                SVELTE_SPANS,
                "",
                "treesitter/svelte",
                "svelte",
            )?,
            vue: LangSpec::new(
                tree_sitter_vue_next::LANGUAGE.into(),
                VUE_QUERY,
                VUE_SPANS,
                "",
                "treesitter/vue",
                "vue",
            )?,
            css: LangSpec::new(
                tree_sitter_css::LANGUAGE.into(),
                CSS_QUERY,
                CSS_SPANS,
                "",
                "treesitter/css",
                "css",
            )?,
            hcl: LangSpec::new(
                tree_sitter_hcl::LANGUAGE.into(),
                "",
                HCL_SPANS,
                "",
                "treesitter/hcl",
                "hcl",
            )?,
            powershell: LangSpec::new(
                tree_sitter_powershell::LANGUAGE.into(),
                POWERSHELL_QUERY,
                POWERSHELL_SPANS,
                "",
                "treesitter/powershell",
                "powershell",
            )?,
            proto: LangSpec::new(
                tree_sitter_proto::LANGUAGE.into(),
                PROTO_QUERY,
                PROTO_SPANS,
                "",
                "treesitter/proto",
                "proto",
            )?,
            graphql: LangSpec::new(
                tree_sitter_graphql::LANGUAGE.into(),
                GRAPHQL_QUERY,
                GRAPHQL_SPANS,
                GRAPHQL_REL,
                "treesitter/graphql",
                "graphql",
            )?,
            sql: LangSpec::new(
                tree_sitter_sequel::LANGUAGE.into(),
                SQL_QUERY,
                SQL_SPANS,
                SQL_REL,
                "treesitter/sql",
                "sql",
            )?,
        })
    }

    /// One parse per file yields definition facts and comment/string spans.
    /// Unsupported languages yield nothing (not an error). `path` is only
    /// consulted by languages whose file *is* a definition (Svelte components).
    pub fn extract(&mut self, language: &str, path: &str, src: &[u8]) -> Extracted {
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
            "bash" => self.bash.extract(src),
            "lua" => self.lua.extract(src),
            "scala" => self.scala.extract(src),
            "kotlin" => self.kotlin.extract(src),
            "swift" => self.swift.extract(src),
            "dart" => self.dart.extract(src),
            "zig" => self.zig.extract(src),
            "elixir" => self.elixir.extract(src),
            "solidity" => self.solidity.extract(src),
            "markdown" => (markdown_facts(src), None),
            "rst" => (rst_facts(src), None),
            "jinja" => (jinja_facts(src), None),
            "css" => self.css.extract(src),
            // Derived rather than queried — see `hcl_facts`.
            "hcl" => {
                let tree = match self.hcl.parser.parse(src, None) {
                    Some(t) => t,
                    None => return ((Vec::new(), Vec::new(), Vec::new()), None),
                };
                let root = tree.root_node();
                let (mut facts, spans, rels) = self.hcl.harvest(root, src);
                facts.extend(hcl_facts(root, src));
                ((facts, spans, rels), Some(parse_health(root)))
            }
            "powershell" => self.powershell.extract(src),
            "proto" => self.proto.extract(src),
            "graphql" => self.graphql.extract(src),
            // Query plus a code pass — see `sql_extras` and `unqualify` for the
            // three things a query cannot do here.
            "sql" => {
                // tree-sitter-sequel does not accept doubled delimiters inside
                // quoted identifiers and does not parse T-SQL bracket quoting.
                // Substitute only escape syntax with same-width identifier bytes
                // so node offsets remain identical, then harvest text/ranges from
                // the untouched source below.
                let repaired = repair_sql_identifier_escapes(src);
                let parse_src = repaired.as_deref().unwrap_or(src);
                let tree = match self.sql.parser.parse(parse_src, None) {
                    Some(t) => t,
                    None => return ((Vec::new(), Vec::new(), Vec::new()), None),
                };
                let root = tree.root_node();
                let (mut facts, mut spans, rels) = self.sql.harvest(root, src);
                let (extra_facts, extra_spans) = sql_extras(root, src);
                facts.extend(extra_facts);
                spans.extend(extra_spans);
                spans.sort_by_key(|(s, _, _)| *s);
                for f in &mut facts {
                    f.name = unqualify(&f.name);
                    f.owner = f.owner.as_deref().map(unqualify);
                }
                ((facts, spans, rels), Some(parse_health(root)))
            }
            // Single-file component formats: markup wrapping the real code. Both
            // grammars name the nodes the same way, so one path serves both —
            // disjoint field borrows let the markup and nested extractors be
            // held mutably at once.
            "svelte" => extract_component(
                &mut self.svelte,
                &mut self.javascript,
                &mut self.typescript,
                path,
                src,
            ),
            "vue" => extract_component(
                &mut self.vue,
                &mut self.javascript,
                &mut self.typescript,
                path,
                src,
            ),
            _ => ((Vec::new(), Vec::new(), Vec::new()), None),
        }
    }
}

/// A single-file component (Svelte `.svelte`, Vue `.vue`). The code an agent
/// cares about lives in `<script>`, which these grammars leave as one opaque
/// `raw_text` token — so each block is re-parsed with the JavaScript or
/// TypeScript extractor (per its `lang` attribute) over a copy of the file with
/// everything *outside* the block blanked out. Blanking rather than slicing keeps
/// every byte offset, line and column in the file's own coordinates, so the
/// nested facts need no remapping and `read`/`at` address them like any other
/// definition.
///
/// `markup` contributes whatever the template itself defines (Svelte's
/// `{#snippet}`; Vue's template defines nothing) plus comment/attribute spans;
/// `<style>` contents stay unclassified (no CSS grammar), and the file itself
/// yields a `component` fact.
fn extract_component(
    markup: &mut LangSpec,
    javascript: &mut LangSpec,
    typescript: &mut LangSpec,
    path: &str,
    src: &[u8],
) -> Extracted {
    let tree = match markup.parser.parse(src, None) {
        Some(t) => t,
        None => return ((Vec::new(), Vec::new(), Vec::new()), None),
    };
    let root = tree.root_node();
    let (mut facts, mut spans, mut relations) = markup.harvest(root, src);
    let mut health = parse_health(root);

    for (start, end, is_ts) in script_regions(root, src) {
        let blanked = blank_outside(src, start, end);
        let ((f, s, r), nested_health) = if is_ts {
            typescript.extract(&blanked)
        } else {
            javascript.extract(&blanked)
        };
        if let Some(nested) = nested_health {
            health.errors += nested.errors;
            health.recovered |= nested.recovered;
        }
        facts.extend(f);
        spans.extend(s);
        relations.extend(r);
    }
    facts.extend(component_fact(path, src));
    spans.sort_by_key(|(s, _, _)| *s);
    ((facts, spans, relations), Some(health))
}

/// The byte range of each `<script>` block's *contents*, plus whether the block
/// declares TypeScript. Walked from the tree rather than queried so the one
/// parse in `extract_svelte` serves both passes.
pub(crate) fn script_regions(root: Node, src: &[u8]) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        if node.kind() == "script_element" {
            if let Some(raw) = node.children(&mut c).find(|n| n.kind() == "raw_text") {
                out.push((raw.start_byte(), raw.end_byte(), script_is_ts(node, src)));
            }
            continue; // contents are one token — nothing further to walk
        }
        for child in node.children(&mut c) {
            stack.push(child);
        }
    }
    out.sort_by_key(|(s, _, _)| *s);
    out
}

/// Whether a `<script>` element is TypeScript, i.e. carries `lang="ts"` (or
/// `lang="typescript"`). Anything else — including no `lang` at all — is
/// JavaScript, which is Svelte's own default.
fn script_is_ts(script: Node, src: &[u8]) -> bool {
    let mut c = script.walk();
    let tag = match script.children(&mut c).find(|n| n.kind() == "start_tag") {
        Some(t) => t,
        None => return false,
    };
    let mut tc = tag.walk();
    for attr in tag.children(&mut tc).filter(|n| n.kind() == "attribute") {
        if first_child_kind(attr, "attribute_name", src) != Some("lang") {
            continue;
        }
        let mut ac = attr.walk();
        let value = attr
            .children(&mut ac)
            .find(|n| matches!(n.kind(), "quoted_attribute_value" | "attribute_value"))
            .and_then(|n| n.utf8_text(src).ok())
            .map(|t| t.trim_matches(|c| c == '"' || c == '\''));
        return matches!(value, Some("ts") | Some("typescript"));
    }
    false
}

// --- Markdown ----------------------------------------------------------------

/// Markdown structure, parsed line-wise rather than with a grammar.
///
/// Docs are the first thing read in an unfamiliar repo and the last thing codeq
/// knew anything about. Treating a **heading as a definition** is the whole
/// feature: a heading has a name, a byte range (down to the next heading of the
/// same or higher level) and a nesting depth, which is exactly the fact model
/// every other language produces. `outline`, `at`, `read --symbol`, the folded
/// `read`, and the enclosing-`[symbol]` annotation on search hits all then work
/// on prose for free.
///
/// Deliberately not Tree-sitter: headings are unambiguous line-wise as long as
/// fenced blocks are tracked (so a `# comment` inside a shell example is not a
/// heading), and this avoids a grammar dependency for the one format where the
/// structure is trivial. `source` records which was used.
///
/// Also emitted:
/// - **Spans.** A fenced code block is `Str`, prose is `Comment`, so
///   `--in strings` searches the examples and `--in comments` searches the prose.
///   Markdown has no "code" in the `--in code` sense, so that filter is
///   correctly empty here rather than quietly matching everything.
/// - **Imports.** A link or image with a local target (`](./x.md)`,
///   `![](assets/logo.png)`, and reference definitions `[l]: ./x.md`) becomes an
///   import, so `deps`, `importers` and cycle detection cover documentation and
///   its assets. External URLs and bare `#anchor` links are not file references
///   and are skipped.
fn markdown_facts(src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    let mut facts: Vec<Fact> = Vec::new();
    let mut spans: Vec<(usize, usize, Span)> = Vec::new();

    // (line_index, byte_start, byte_end_excluding_newline, text)
    let mut lines: Vec<(usize, usize, &str)> = Vec::new();
    let mut off = 0usize;
    for l in text.split_inclusive('\n') {
        let trimmed = l.strip_suffix('\n').unwrap_or(l);
        let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        lines.push((off, off + trimmed.len(), trimmed));
        off += l.len();
    }

    // A heading found in the scan: (line index, level, name, byte start).
    struct Head {
        line: usize,
        level: usize,
        name: String,
        start: usize,
    }
    let mut heads: Vec<Head> = Vec::new();

    // Prose/code classification, accumulated as runs and merged at the end.
    let mut classes: Vec<Option<Span>> = vec![None; lines.len()];

    let mut fence: Option<(char, usize)> = None; // (char, run length)
    let mut in_frontmatter = false;

    for (i, (start, end, raw)) in lines.iter().enumerate() {
        let t = raw.trim_start();
        let indent = raw.len() - t.len();

        // YAML/TOML frontmatter at the very top is metadata, not prose or
        // headings; its `---` must not be read as a setext underline.
        if i == 0 && (t == "---" || t == "+++") {
            in_frontmatter = true;
            classes[i] = Some(Span::Str);
            continue;
        }
        if in_frontmatter {
            classes[i] = Some(Span::Str);
            if t == "---" || t == "+++" {
                in_frontmatter = false;
            }
            continue;
        }

        // Fenced code blocks. A closing fence is the same character, at least as
        // long as the opener, and carries no info string.
        let fence_run = |s: &str, c: char| s.chars().take_while(|&x| x == c).count();
        if let Some((fc, flen)) = fence {
            classes[i] = Some(Span::Str);
            let run = fence_run(t, fc);
            if run >= flen && t[run..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if indent < 4 && (t.starts_with("```") || t.starts_with("~~~")) {
            let fc = t.chars().next().unwrap();
            fence = Some((fc, fence_run(t, fc)));
            classes[i] = Some(Span::Str);
            continue;
        }

        classes[i] = Some(Span::Comment); // ordinary prose

        // ATX heading: up to 3 spaces of indent, 1-6 `#`, then a space.
        if indent < 4 && t.starts_with('#') {
            let level = t.chars().take_while(|&c| c == '#').count();
            let rest = &t[level..];
            if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(' ')) {
                // A trailing run of `#` is decoration, not part of the name.
                let name = rest.trim().trim_end_matches('#').trim().to_string();
                if !name.is_empty() {
                    heads.push(Head {
                        line: i,
                        level,
                        name,
                        start: *start,
                    });
                }
                continue;
            }
        }

        // Setext underline: `===` or `---` directly under paragraph text. The
        // preceding line must be ordinary prose, otherwise `---` is a thematic
        // break (or a list marker), not a heading.
        if indent < 4
            && !t.is_empty()
            && (t.chars().all(|c| c == '=') || t.chars().all(|c| c == '-'))
        {
            if let Some(prev) = i.checked_sub(1) {
                let prev_raw = lines[prev].2.trim();
                let prev_is_prose = !prev_raw.is_empty()
                    && classes[prev] == Some(Span::Comment)
                    && !heads.iter().any(|h| h.line == prev)
                    && !prev_raw.starts_with(['-', '*', '+', '>', '#']);
                if prev_is_prose {
                    let level = if t.starts_with('=') { 1 } else { 2 };
                    heads.push(Head {
                        line: prev,
                        level,
                        name: prev_raw.to_string(),
                        start: lines[prev].0,
                    });
                    continue;
                }
            }
        }

        // File references: inline links/images and reference definitions.
        collect_md_links(raw, *start, i + 1, &mut facts);
        let _ = end;
    }

    // A heading owns everything down to the next heading of the same or higher
    // level, which is what makes containment (and therefore outline nesting)
    // work without tracking a stack.
    let eof = text.len();
    for (idx, h) in heads.iter().enumerate() {
        let end_byte = heads[idx + 1..]
            .iter()
            .find(|o| o.level <= h.level)
            .map(|o| o.start)
            .unwrap_or(eof);
        let end_line = heads[idx + 1..]
            .iter()
            .find(|o| o.level <= h.level)
            .map(|o| o.line)
            .unwrap_or(lines.len());
        // The section's first prose line doubles as its summary, the way a
        // docstring does for a function.
        let doc = lines[(h.line + 1).min(lines.len())..end_line]
            .iter()
            .map(|(_, _, t)| t.trim())
            .find(|t| !t.is_empty())
            .map(|t| cap(&collapse_ws(t), MAX_DOC));
        facts.push(Fact {
            kind: "heading".to_string(),
            is_derived: false,
            name: h.name.clone(),
            start_byte: h.start,
            end_byte: end_byte.max(h.start),
            start_line: h.line + 1,
            start_col: 0,
            end_line: end_line.max(h.line + 1),
            end_col: 0,
            signature: cap(&collapse_ws(lines[h.line].2.trim()), MAX_SIG),
            doc,
            source: "lines/markdown".to_string(),
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

    // Merge the per-line classes into runs.
    let mut i = 0usize;
    while i < lines.len() {
        let c = classes[i];
        if c.is_none() {
            i += 1;
            continue;
        }
        let start = lines[i].0;
        let mut j = i;
        while j + 1 < lines.len() && classes[j + 1] == c {
            j += 1;
        }
        let end = if j + 1 < lines.len() {
            lines[j + 1].0
        } else {
            eof
        };
        spans.push((start, end, c.unwrap()));
        i = j + 1;
    }

    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (facts, spans, Vec::new())
}

/// Local file references on one markdown line: inline `[t](path)` / `![a](path)`
/// and reference definitions `[label]: path`. External URLs and pure `#anchor`
/// links name no file, so they carry no edge.
///
/// Anything inside an inline code span is an *example* of a link, not a link:
/// documentation about markdown is full of `` `[t](./guide.md)` `` that must not
/// become a dependency edge (or a broken one). Found by dogfooding, when
/// `deps --external` reported this repo's own README as having two dead links.
fn collect_md_links(raw: &str, line_start: usize, line_no: usize, facts: &mut Vec<Fact>) {
    let mut push = |target: &str, at: usize| {
        let t = target.trim();
        // Strip an optional title: `(path "Title")`.
        let t = t.split_whitespace().next().unwrap_or(t);
        let t = t.trim_matches(|c| c == '<' || c == '>');
        // Drop any anchor, keeping the file part (`./x.md#section` -> `./x.md`).
        let file = t.split('#').next().unwrap_or(t);
        if file.is_empty() || is_external_url(t) {
            return;
        }
        facts.push(Fact {
            kind: "import".to_string(),
            is_derived: false,
            name: file.to_string(),
            start_byte: line_start + at,
            end_byte: line_start + at + target.len(),
            start_line: line_no,
            start_col: at,
            end_line: line_no,
            end_col: at + target.len(),
            signature: String::new(),
            doc: None,
            source: "lines/markdown".to_string(),
            confidence: 1.0,
            visibility: None,
            owner: None,
            shape: None,
            shape_nodes: 0,
            is_test: false,
            decorator: None,
            contract: None,
        });
    };

    // Byte ranges covered by inline code spans on this line.
    let code_spans = inline_code_spans(raw);
    let in_code = |at: usize| code_spans.iter().any(|(s, e)| at >= *s && at < *e);

    let bytes = raw.as_bytes();
    // Reference definition: `[label]: target`.
    if let Some(rest) = raw.trim_start().strip_prefix('[') {
        if let Some((_, after)) = rest.split_once("]:") {
            let at = raw.len() - after.len();
            if !in_code(at) {
                push(after, at);
            }
            return;
        }
    }
    // Inline `](target)`, which covers both links and images.
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            if let Some(close) = raw[i + 2..].find(')') {
                if !in_code(i) {
                    push(&raw[i + 2..i + 2 + close], i + 2);
                }
                i += 2 + close;
                continue;
            }
        }
        i += 1;
    }
}

/// Byte ranges of inline code spans (`` `x` ``, ``` ``x`` ```) on one line.
fn inline_code_spans(raw: &str) -> Vec<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        let open = i;
        let mut j = i + ticks;
        // The closing run must be the same length, as in CommonMark.
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let run = bytes[j..].iter().take_while(|&&b| b == b'`').count();
                if run == ticks {
                    spans.push((open, j + run));
                    break;
                }
                j += run;
            } else {
                j += 1;
            }
        }
        i = if j < bytes.len() {
            j + ticks
        } else {
            bytes.len()
        };
    }
    spans
}

fn is_external_url(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("ftp://")
        || lower.starts_with("//")
        || t.starts_with('#')
}

// --- reStructuredText -------------------------------------------------------

/// RST's structure is simple enough to extract line-wise, as Markdown's is:
/// a heading is text immediately followed by a repeated adornment character.
/// The first appearance of an adornment determines its nesting level, which is
/// the convention Docutils uses in practice. Local document roles and include,
/// image, and hyperlink-target directives become imports so Flask's docs take
/// part in `outline`, `read`, and `deps` instead of being opaque assets.
fn rst_facts(src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    let mut lines: Vec<(usize, usize, &str)> = Vec::new();
    let mut off = 0usize;
    for line in text.split_inclusive('\n') {
        let raw = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or(line);
        lines.push((off, off + raw.len(), raw));
        off += line.len();
    }
    struct Head {
        line: usize,
        level: usize,
        name: String,
        start: usize,
    }
    let mut heads = Vec::<Head>::new();
    let mut adornments = Vec::<char>::new();
    let mut facts = Vec::new();
    let mut toctree_indent: Option<usize> = None;
    for (i, (start, _, raw)) in lines.iter().enumerate() {
        let t = raw.trim();
        let indent = raw.len() - raw.trim_start().len();
        if let Some(base_indent) = toctree_indent {
            if t.is_empty() || t.starts_with(':') {
                // Blank lines and directive options belong to the toctree.
            } else if indent > base_indent {
                if let Some(module) = rst_import_target(t, true) {
                    facts.push(line_fact(
                        "import",
                        &module,
                        *start + indent,
                        *start + indent + t.len(),
                        i + 1,
                        i + 1,
                        String::new(),
                        "lines/rst",
                    ));
                }
                continue;
            } else {
                toctree_indent = None;
            }
        }
        if t.starts_with(".. toctree::") {
            toctree_indent = Some(indent);
        }
        if i > 0 && is_rst_adornment(t) {
            let prev = lines[i - 1].2.trim();
            if !prev.is_empty() && !is_rst_adornment(prev) {
                let marker = t.chars().next().unwrap();
                let level = match adornments.iter().position(|c| *c == marker) {
                    Some(n) => n + 1,
                    None => {
                        adornments.push(marker);
                        adornments.len()
                    }
                };
                heads.push(Head {
                    line: i - 1,
                    level,
                    name: prev.to_string(),
                    start: lines[i - 1].0,
                });
            }
        }
        collect_rst_links(raw, *start, i + 1, &mut facts);
    }
    let eof = text.len();
    for (i, head) in heads.iter().enumerate() {
        let next = heads[i + 1..]
            .iter()
            .find(|other| other.level <= head.level);
        let end_byte = next.map(|h| h.start).unwrap_or(eof);
        let end_line = next.map(|h| h.line + 1).unwrap_or(lines.len());
        facts.push(line_fact(
            "heading",
            &head.name,
            head.start,
            end_byte,
            head.line + 1,
            end_line,
            cap(&collapse_ws(lines[head.line].2), MAX_SIG),
            "lines/rst",
        ));
    }
    let spans = (!src.is_empty())
        .then(|| vec![(0, src.len(), Span::Comment)])
        .unwrap_or_default();
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (facts, spans, Vec::new())
}

fn is_rst_adornment(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() >= 3
        && matches!(
            first,
            '=' | '-' | '~' | '^' | '"' | '\'' | '`' | ':' | '.' | '*' | '+' | '#'
        )
        && chars.all(|c| c == first)
}

fn collect_rst_links(raw: &str, line_start: usize, line_no: usize, facts: &mut Vec<Fact>) {
    let mut push = |target: &str, at: usize, doc_role: bool| {
        let Some(module) = rst_import_target(target, doc_role) else {
            return;
        };
        facts.push(line_fact(
            "import",
            &module,
            line_start + at,
            line_start + at + target.len(),
            line_no,
            line_no,
            String::new(),
            "lines/rst",
        ));
    };
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    for directive in [".. include::", ".. image::", ".. figure::"] {
        if let Some(target) = trimmed.strip_prefix(directive) {
            let at = indent + directive.len();
            push(target, at, false);
            return;
        }
    }
    if let Some(rest) = trimmed.strip_prefix(".. _") {
        if let Some((_, target)) = rest.split_once(':') {
            push(target, raw.len() - target.len(), false);
        }
    }
    let mut from = 0usize;
    while let Some(pos) = raw[from..].find(":doc:`") {
        let start = from + pos + 6;
        if let Some(end) = raw[start..].find('`') {
            push(&raw[start..start + end], start, true);
            from = start + end + 1;
        } else {
            break;
        }
    }
    // Conventional RST hyperlink: `caption <relative/file.rst>`_.
    let mut from = 0usize;
    while let Some(pos) = raw[from..].find('<') {
        let start = from + pos + 1;
        if let Some(end) = raw[start..].find(">`_") {
            push(&raw[start..start + end], start, false);
            from = start + end + 3;
        } else {
            break;
        }
    }
}

fn rst_import_target(target: &str, doc_role: bool) -> Option<String> {
    let target = target
        .trim()
        .trim_matches(|c| c == '`' || c == '"' || c == '\'');
    let target = target
        .rsplit_once('<')
        .and_then(|(_, tail)| tail.strip_suffix('>'))
        .unwrap_or(target);
    let target = target
        .trim_start_matches('~')
        .split('#')
        .next()
        .unwrap_or(target)
        .trim();
    if target.is_empty() || is_external_url(target) || target.starts_with(':') {
        return None;
    }
    Some(
        if doc_role && !target.rsplit('/').next().unwrap_or(target).contains('.') {
            format!("{target}.rst")
        } else {
            target.to_string()
        },
    )
}

// --- Jinja ------------------------------------------------------------------

/// Template files are not general HTML: the useful repository structure is in
/// Jinja statements. Extract macros and blocks as definitions, and template
/// composition (`extends`, `include`, and macro imports) as local file edges.
/// A small scanner is intentionally preferable to an HTML grammar here: it
/// preserves byte locations and handles Flask's `.html` templates as well as
/// `.j2`/`.jinja` files without pretending to understand rendered HTML.
fn jinja_facts(src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    struct Open {
        kind: &'static str,
        name: String,
        start: usize,
        line: usize,
        signature: String,
    }
    let mut open = Vec::<Open>::new();
    let mut facts = Vec::new();
    let mut at = 0usize;
    while let Some(rel) = text[at..].find("{%") {
        let start = at + rel;
        let Some(close_rel) = text[start + 2..].find("%}") else {
            break;
        };
        let end = start + 2 + close_rel + 2;
        let body = text[start + 2..start + 2 + close_rel].trim();
        let line = line_number(src, start);
        let word = body.split_whitespace().next().unwrap_or("");
        match word {
            "extends" | "include" | "import" | "from" => {
                if let Some((target, offset)) = jinja_quoted_target(body) {
                    facts.push(line_fact(
                        "import",
                        target,
                        start + 2 + offset,
                        start + 2 + offset + target.len(),
                        line,
                        line,
                        String::new(),
                        "lines/jinja",
                    ));
                }
            }
            "block" | "macro" => {
                if let Some(name) = body.split_whitespace().nth(1) {
                    let name = name.split('(').next().unwrap_or(name).trim_end_matches('-');
                    if !name.is_empty() {
                        open.push(Open {
                            kind: if word == "block" { "block" } else { "function" },
                            name: name.to_string(),
                            start,
                            line,
                            signature: format!("{{% {body} %}}"),
                        });
                    }
                }
            }
            "endblock" | "endmacro" => {
                let kind = if word == "endblock" {
                    "block"
                } else {
                    "function"
                };
                if let Some(pos) = open.iter().rposition(|o| o.kind == kind) {
                    let entry = open.remove(pos);
                    facts.push(line_fact(
                        entry.kind,
                        &entry.name,
                        entry.start,
                        end,
                        entry.line,
                        line,
                        entry.signature,
                        "lines/jinja",
                    ));
                }
            }
            _ => {}
        }
        at = end;
    }
    let eof_line = line_number(src, src.len());
    for entry in open {
        facts.push(line_fact(
            entry.kind,
            &entry.name,
            entry.start,
            src.len(),
            entry.line,
            eof_line,
            entry.signature,
            "lines/jinja",
        ));
    }
    let spans = (!src.is_empty())
        .then(|| vec![(0, src.len(), Span::Comment)])
        .unwrap_or_default();
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (facts, spans, Vec::new())
}

fn jinja_quoted_target(body: &str) -> Option<(&str, usize)> {
    for quote in ['\'', '"'] {
        let Some(start) = body.find(quote) else {
            continue;
        };
        if let Some(end) = body[start + 1..].find(quote) {
            return Some((&body[start + 1..start + 1 + end], start + 1));
        }
    }
    None
}

fn line_number(src: &[u8], offset: usize) -> usize {
    src[..offset.min(src.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn line_fact(
    kind: &str,
    name: &str,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
    signature: String,
    source: &str,
) -> Fact {
    Fact {
        kind: kind.to_string(),
        is_derived: false,
        name: name.to_string(),
        start_byte,
        end_byte,
        start_line,
        start_col: 0,
        end_line,
        end_col: 0,
        signature,
        doc: None,
        source: source.to_string(),
        confidence: 1.0,
        visibility: None,
        owner: None,
        shape: None,
        shape_nodes: 0,
        is_test: false,
        decorator: None,
        contract: None,
    }
}

/// Whether a function node is a method — declared inside a type body, so it can
/// be called on a value.
///
/// Walks outward and stops at the first enclosing definition: a function ancestor
/// means this is a nested function or closure, not a method, however deeply the
/// class sits above it.
///
/// Rust is the exception that needs more than nesting. An `impl` block holds both
/// methods and associated functions, and only a `self` parameter distinguishes
/// `s.meth()` from `S::assoc()`, so Rust requires one.
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

/// A copy of `src` with every byte outside `[start, end)` replaced by a space,
/// newlines excepted. Parsing this instead of the slice keeps the embedded
/// region at its original byte offsets, rows and columns, so facts extracted
/// from it are already in file coordinates.
pub(crate) fn blank_outside(src: &[u8], start: usize, end: usize) -> Vec<u8> {
    let mut buf = src.to_vec();
    for (i, b) in buf.iter_mut().enumerate() {
        if (i < start || i >= end) && *b != b'\n' {
            *b = b' ';
        }
    }
    buf
}

/// A `.svelte` file *is* a component, named by its file stem and used as
/// `<Button …/>` after `import Button from './Button.svelte'` — there is no
/// declaration in the source to capture, so the file itself is the definition.
/// SvelteKit route files (`+page.svelte`, `+layout.svelte`) are loaded by
/// convention and never referenced by name, so they get none: a fact there could
/// only ever look unused.
fn component_fact(path: &str, src: &[u8]) -> Option<Fact> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    if stem.is_empty() || stem.starts_with('+') {
        return None;
    }
    let newlines = src.iter().filter(|b| **b == b'\n').count();
    let last_line_start = src.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    Some(Fact {
        kind: "component".to_string(),
        is_derived: false,
        name: stem.to_string(),
        start_byte: 0,
        end_byte: src.len(),
        start_line: 1,
        start_col: 0,
        end_line: newlines + 1,
        end_col: src.len() - last_line_start,
        signature: format!("<{stem} />"),
        doc: None,
        // Not a captured node: derived from the file's own identity, and marked
        // as such so the provenance stays honest.
        source: "svelte/component".to_string(),
        confidence: 1.0,
        visibility: Some("public".to_string()), // importable by any other file
        owner: None,
        shape: None,
        shape_nodes: 0,
        is_test: false,
        decorator: None,
        contract: None,
    })
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

/// Declarations that exist to be consumed from *outside* the repository, so
/// "nothing here references it" is their normal state rather than a finding.
/// `None` covers a whole language; `Some(kind)` narrows to one kind within it.
///
/// A table here rather than a hand-written clause in `cmd_unused`'s SQL, which
/// is where this policy used to live. It has grown once per language with an
/// external contract — GraphQL, protobuf, Terraform outputs, now SQL — and that
/// is a category that keeps appearing, so it belongs next to everything else
/// that describes a language instead of inside a query string in another file.
pub const UNREFERENCED_BY_DESIGN: &[(&str, Option<&str>)] = &[
    // A GraphQL type or a protobuf message is a contract for another codebase.
    ("graphql", None),
    ("proto", None),
    // A Terraform `output` exists precisely so something outside can read it.
    ("hcl", Some("output")),
    // A SQL index or trigger is named exactly once in the world, at its own
    // `CREATE`, and an enum variant is referenced by data rather than by code.
    // The rest of SQL is not here: a table, view, column or function *is* named
    // by whatever queries it, and codeq finds those names inside the SQL strings
    // in application code, so an unreferenced one is a real finding.
    ("sql", Some("index")),
    ("sql", Some("trigger")),
    ("sql", Some("variant")),
];

/// Names the *runtime* reads and code never references. `__all__` is declared in
/// every Python package that curates its exports and named nowhere else, by
/// construction — the same sentence that already excludes SQL indexes and
/// triggers. A short list rather than a blanket dunder rule: `__version__` *is*
/// referenced, routinely, and hiding it would be the opposite mistake.
pub const RUNTIME_READ_NAMES: &[(&str, &str)] = &[
    ("python", "__all__"),
    ("python", "__path__"),
    ("python", "__getattr__"),
];

/// Python protocol and framework hooks that execute through the interpreter,
/// dataclasses, or Pydantic rather than an ordinary name lookup. Keep this
/// explicit: `__version__` and arbitrary dunder names remain candidates unless
/// codeq observes a real use.
pub const PYTHON_RUNTIME_HOOKS: &[&str] = &[
    "__aenter__",
    "__aexit__",
    "__aiter__",
    "__anext__",
    "__await__",
    "__bool__",
    "__bytes__",
    "__call__",
    "__class_getitem__",
    "__contains__",
    "__delattr__",
    "__delitem__",
    "__delete__",
    "__dir__",
    "__enter__",
    "__eq__",
    "__exit__",
    "__format__",
    "__get__",
    "__get_pydantic_json_schema__",
    "__get_validators__",
    "__getattribute__",
    "__getitem__",
    "__hash__",
    "__init__",
    "__init_subclass__",
    "__iter__",
    "__len__",
    "__new__",
    "__post_init__",
    "__repr__",
    "__reversed__",
    "__set__",
    "__set_name__",
    "__setattr__",
    "__setitem__",
    "__slots__",
    "__str__",
];

/// JavaScript-family constructors are invoked as `new TypeName()`, not by the
/// method name `constructor`. A name-based reference scan therefore cannot
/// distinguish an instantiated class from one whose constructor is dead; the
/// class itself remains eligible for `unused`.
pub const UNREFERENCED_CONSTRUCTORS: &[(&str, &str, &str)] = &[
    ("javascript", "method", "constructor"),
    ("typescript", "method", "constructor"),
    ("tsx", "method", "constructor"),
];

/// Whether `unused` should stay silent about a definition because nothing in
/// this repository was ever going to reference it. See [`UNREFERENCED_BY_DESIGN`].
pub fn unreferenced_by_design(language: Option<&str>, kind: &str, name: &str) -> bool {
    let Some(lang) = language else { return false };
    UNREFERENCED_BY_DESIGN
        .iter()
        .any(|(l, k)| *l == lang && k.is_none_or(|k| k == kind))
        || RUNTIME_READ_NAMES.iter().any(|(l, n)| *l == lang && *n == name)
        || (lang == "python" && PYTHON_RUNTIME_HOOKS.contains(&name))
        // A Jinja block is filled by template inheritance and dispatched by the
        // renderer. There is no source-level call site for an override, so it
        // is an entry point rather than a dead definition.
        || (lang == "jinja" && kind == "block")
        || UNREFERENCED_CONSTRUCTORS
            .iter()
            .any(|(l, k, n)| *l == lang && *k == kind && *n == name)
}

/// Languages that can carry a JavaScript framework's file conventions.
const WEB_LANGUAGES: &[&str] = &["javascript", "typescript", "tsx", "jsx"];

/// React class-component methods the runtime calls. The class names them in no
/// other place, and unlike a route there is no decorator to notice.
const REACT_LIFECYCLE: &[&str] = &[
    "render",
    "componentDidMount",
    "componentDidUpdate",
    "componentWillUnmount",
    "shouldComponentUpdate",
    "getSnapshotBeforeUpdate",
    "componentDidCatch",
    "getDerivedStateFromError",
    "getDerivedStateFromProps",
];

/// The framework that reaches this definition without any call naming it, or
/// `None`.
///
/// [`Fact::decorator`] covers registration codeq can *see* — `@app.get("/x")`
/// sits on the line above. A file convention leaves no such mark: Next.js loads
/// the default export of `app/**/page.tsx` because of where the file sits and
/// what it is called, so on an App Router repo every route, layout and metadata
/// export read as dead code and `entrypoints` reported none at all.
///
/// Tag, not suppress. Deleting a `page.tsx` export still compiles — the route
/// 404s — so this is the "looks dead, might not be" case the `decorator=` tag
/// exists for, and not the never-dead case of a trait implementation.
///
/// A convention file's whole public surface is the framework's contract
/// (`metadata` and `viewport` beside the component), and codeq does not record
/// which export was `default`, so the public surface is what gets tagged.
pub fn framework_registration(
    language: Option<&str>,
    kind: &str,
    name: &str,
    path: &str,
    visibility: Option<&str>,
) -> Option<&'static str> {
    if !language.is_some_and(|l| WEB_LANGUAGES.contains(&l)) {
        return None;
    }
    if kind == "method" && REACT_LIFECYCLE.contains(&name) {
        return Some("react/lifecycle");
    }
    // Everything below is a module-level export the framework loads by path.
    if visibility != Some("public") {
        return None;
    }
    let mut segments: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let file = segments.pop()?;
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let dirs = segments;

    // `middleware.ts` and `instrumentation.ts` are only conventional at the
    // project root, where `src/` is the one directory Next.js also allows.
    if dirs.is_empty() || dirs == ["src"] {
        match stem {
            "middleware" => return Some("next/middleware"),
            "instrumentation" => return Some("next/instrumentation"),
            _ => {}
        }
    }
    // The Pages Router routes *every* file under `pages/`, so the file name
    // carries no further information.
    if dirs.contains(&"pages") {
        return Some("next/pages-router");
    }
    if !dirs.contains(&"app") {
        return None;
    }
    Some(match stem {
        "page" => "next/page",
        "layout" => "next/layout",
        "route" => "next/route",
        "template" => "next/template",
        "loading" => "next/loading",
        "error" | "global-error" => "next/error",
        "not-found" => "next/not-found",
        "default" => "next/default",
        "sitemap" => "next/sitemap",
        "robots" => "next/robots",
        "manifest" => "next/manifest",
        "icon" | "apple-icon" | "opengraph-image" | "twitter-image" => "next/metadata-image",
        _ => return None,
    })
}

/// What codeq can extract for one language.
///
/// Support is uneven and always has been — Go has no inheritance edges, Ruby
/// declares no fields, several languages have no import syntax codeq reads — but
/// `capabilities` presented flat lists, so "no results" and "not supported"
/// looked identical. This makes the difference queryable.
///
/// Derived from each language's own query text rather than a hand-kept table,
/// because the hand-kept list is exactly what drifted (it listed `field`/
/// `variant` twice and omitted `heading`).
#[derive(serde::Serialize)]
pub struct Support {
    pub language: String,
    /// Definitions: functions, types, and so on.
    pub definitions: bool,
    /// Import statements, so `deps`/`importers` work.
    pub imports: bool,
    /// Fields and enum variants, for `members`.
    pub members: bool,
    /// Inheritance edges, for `implementors`/`supertypes`.
    pub relations: bool,
    /// Which relation kinds this language's query can actually produce.
    ///
    /// `relations: true` is too coarse to act on. C# writes `class C : B, I`
    /// for both inheritance and implementation and the grammar does not
    /// distinguish them, so codeq can only ever emit `extends` there — and
    /// `graph --relation implements` on a C# repo full of interface
    /// implementations comes back empty for a reason that has nothing to do
    /// with the code. Derived from the query's own capture names, like every
    /// other capability here, because the hand-kept list is what drifts.
    pub relation_kinds: Vec<String>,
    /// Comment/string spans, for `search --in`.
    pub spans: bool,
    /// Call sites, for the `--calls`/`--on` precision filters.
    pub calls: bool,
}

/// The raw query text for a language: (definitions, relations).
fn queries_for(language: &str) -> Option<(String, String)> {
    let pair = |q: &str, r: &str| Some((q.to_string(), r.to_string()));
    match language {
        "rust" => pair(RUST_QUERY, RUST_REL),
        "python" => pair(PYTHON_QUERY, PYTHON_REL),
        "javascript" => pair(&format!("{JS_QUERY}\n{JS_EXTRA}"), JS_REL),
        "typescript" | "tsx" => pair(&format!("{JS_QUERY}\n{TS_EXTRA}"), TS_REL),
        "go" => pair(GO_QUERY, GO_REL),
        "java" => pair(JAVA_QUERY, JAVA_REL),
        "ruby" => pair(RUBY_QUERY, RUBY_REL),
        "c" => pair(C_QUERY, C_REL),
        "cpp" => pair(CPP_QUERY, CPP_REL),
        "csharp" => pair(CSHARP_QUERY, CSHARP_REL),
        "php" => pair(PHP_QUERY, PHP_REL),
        "bash" => pair(BASH_QUERY, ""),
        "lua" => pair(LUA_QUERY, ""),
        "scala" => pair(SCALA_QUERY, SCALA_REL),
        "kotlin" => pair(KOTLIN_QUERY, KOTLIN_REL),
        "swift" => pair(SWIFT_QUERY, SWIFT_REL),
        "dart" => pair(DART_QUERY, DART_REL),
        "zig" => pair(ZIG_QUERY, ""),
        "elixir" => pair(ELIXIR_QUERY, ""),
        "solidity" => pair(SOLIDITY_QUERY, SOLIDITY_REL),
        "css" => pair(CSS_QUERY, ""),
        "powershell" => pair(POWERSHELL_QUERY, ""),
        "proto" => pair(PROTO_QUERY, ""),
        "graphql" => pair(GRAPHQL_QUERY, GRAPHQL_REL),
        "sql" => pair(SQL_QUERY, SQL_REL),
        // svelte, vue, hcl and markdown build their facts in code rather than
        // from a query, so there is no query text to describe them; `support`
        // states their capabilities directly.
        _ => None,
    }
}

pub fn support(language: &str) -> Option<Support> {
    // Four languages build their facts in code rather than from a query, so
    // there is no query text to derive their capabilities from. Describe them
    // directly — the alternative (falling through to `queries_for` and getting
    // `None`) is what made them look unsupported.
    let hand = |definitions, imports, members, relations: bool, calls| {
        Some(Support {
            language: language.to_string(),
            definitions,
            imports,
            members,
            relations,
            // A component's relations come from its embedded `<script>`, so its
            // kinds are TypeScript's.
            relation_kinds: if relations {
                relation_kinds_of(TS_REL)
            } else {
                Vec::new()
            },
            spans: has_spans(language),
            calls,
        })
    };
    match language {
        // Headings are definitions; local links and images are imports; spans
        // separate fenced code from prose. RST has the same useful surface,
        // expressed with adornments and directives rather than Markdown syntax.
        "markdown" | "rst" => return hand(true, true, false, false, false),
        // Blocks and macros are template definitions; extends/include/import
        // directives are local file dependencies.
        "jinja" => return hand(true, true, false, false, false),
        // Labelled blocks and `locals` entries are definitions; a module's
        // `source` is an import. No members, no inheritance.
        "hcl" => return hand(true, true, false, false, false),
        // A component file is itself a definition, and the `<script>` body is
        // extracted with the JS/TS queries — so imports, members and relations
        // all come through from the embedded code.
        // Their markup grammars keep scripts opaque, but callsite extraction
        // reparses each structural `<script>` region as JS/TS at file offsets.
        "svelte" | "vue" => return hand(true, true, true, true, true),
        _ => {}
    }
    let (query, rel) = queries_for(language)?;
    Some(Support {
        language: language.to_string(),
        definitions: query.contains(") @") || query.contains("@function"),
        imports: query.contains("@import"),
        members: query.contains("@field") || query.contains("@variant"),
        relations: !rel.trim().is_empty(),
        relation_kinds: relation_kinds_of(&rel),
        spans: has_spans(language),
        calls: crate::astgrep::ts_language(language).is_some(),
    })
}

/// The relation kinds a relation query can emit, read off its capture names.
fn relation_kinds_of(rel: &str) -> Vec<String> {
    let mut kinds: Vec<String> = ["implements", "extends", "references"]
        .iter()
        .filter(|k| rel.contains(&format!("@{k}")))
        .map(|k| k.to_string())
        .collect();
    kinds.sort();
    kinds
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
            | "bash"
            | "lua"
            | "scala"
            | "kotlin"
            | "swift"
            | "dart"
            | "zig"
            | "elixir"
            | "solidity"
            | "svelte"
            | "vue"
            | "css"
            | "hcl"
            | "powershell"
            | "proto"
            | "graphql"
            | "markdown"
            | "sql"
    )
}

/// The declaration text with any body stripped, collapsed to one line and
/// capped. When there is no body capture (structs, consts, imports, …) the
/// whole node text is used, still capped.
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
        if TYPE_KINDS.contains(&n.kind()) {
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
        if TYPE_KINDS.contains(&n.kind()) {
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

/// Put back a hint of what a folded multi-line value *is*.
///
/// A constant's signature stops where its value begins, which reads well when a
/// wrapper survives the cut (`const Card = memo(`) and badly when nothing does:
/// a 52-entry dict — the substance of its file — rendered as a bare `alias =`,
/// which looks like the renderer gave up rather than like a deliberate fold.
/// Appending the bracket the value opens with costs three characters and says
/// which kind of thing was folded.
fn with_value_shape(sig: String, kind: &str, val: Option<Node>) -> String {
    if kind != "constant" || !sig.trim_end().ends_with('=') {
        return sig;
    }
    let Some(v) = val else { return sig };
    let shape = match v.kind() {
        "dictionary" | "object" | "set" | "block_mapping" => "{…}",
        "list" | "array" | "list_display" => "[…]",
        "tuple" | "arguments" | "parenthesized_expression" => "(…)",
        _ => return sig,
    };
    format!("{} {shape}", sig.trim_end())
}

/// Whether a definition carries a test attribute — the structural answer to a
/// question codeq had only ever answered from spelling.
///
/// Three shapes, because grammars disagree about where an annotation lives:
/// Rust puts `attribute_item` *before* the item as a sibling, Python wraps the
/// definition in a `decorated_definition` *above* it, and Java/Kotlin/C#/PHP hang
/// `modifiers`/`attribute_list` *inside* the declaration. All three are checked;
/// a language with none of them simply yields nothing, which leaves the existing
/// path- and name-based signals to do the work as before.
fn test_attributed(lang: &str, node: Node, src: &[u8]) -> bool {
    let attrs = attributes_of(node, src);
    !attrs.is_empty() && is_test_marker(lang, &attrs.join("\n"))
}

/// Every attribute / decorator / annotation attached to a definition, as written.
///
/// Three shapes, because grammars disagree about where one lives: Rust puts
/// `attribute_item` *before* the item as a sibling, Python wraps the definition
/// in a `decorated_definition` *above* it, and Java/Kotlin/C#/PHP hang
/// `modifiers`/`attribute_list` *inside* the declaration. A language with none of
/// them yields nothing, which is a true answer rather than a failure.
fn attributes_of(node: Node, src: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: Node| {
        if let Ok(t) = n.utf8_text(src) {
            out.push(collapse_ws(t));
        }
    };

    // Rust: preceding siblings, innermost first — collected in reverse so the
    // list reads in source order.
    let mut before: Vec<Node> = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if !p.kind().contains("attribute") {
            break;
        }
        before.push(p);
        prev = p.prev_named_sibling();
    }
    for n in before.into_iter().rev() {
        push(n);
    }
    // Python: on a wrapper node above the definition.
    if let Some(parent) = node.parent() {
        if parent.kind().contains("decorated") {
            let mut c = parent.walk();
            for ch in parent.named_children(&mut c) {
                if ch.kind() == "decorator" {
                    push(ch);
                }
            }
        }
    }
    // Java / Kotlin / C# / PHP: inside the declaration. Matched on the *list*
    // node each grammar wraps annotations in (`attribute_list` in C# and PHP,
    // `modifiers` in Java and Kotlin), never on a bare `attribute` — because in
    // Python that kind is attribute *access*, so `self._axes_visible = True`
    // came back tagged `decorator=self._axes_visible`. The `decorator` field
    // exists so a reader can tell a framework entry point from dead code, and a
    // tag on an ordinary assignment is worse than no tag: it means the tag no
    // longer means anything when it appears.
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        let k = ch.kind();
        if k == "modifiers" {
            // Java and Kotlin put annotations and access modifiers in one node,
            // so pushing it whole reported `decorator=public` on every plain
            // `public class` — a tag that says "a framework may be registering
            // this" on a declaration nothing registers. Take the annotations.
            let mut mc = ch.walk();
            for m in ch.named_children(&mut mc) {
                if m.kind().contains("annotation") {
                    push(m);
                }
            }
        } else if k == "attributes"
            || k.contains("annotation")
            || k.ends_with("attribute_list")
            || k.ends_with("attribute_group")
        {
            push(ch);
        }
    }
    out
}

/// The decorator that might be *registering* this definition somewhere codeq
/// cannot see — a FastAPI `@app.get(...)`, a `@click.command()`, a plugin
/// `@register`. `None` when the definition carries none, or carries only inert
/// ones.
///
/// This exists because `unused` was 100% noise on a decorator-driven codebase: a
/// route's only "reference" is the line directly above it, so every entry point
/// in every Python web app, JS decorator framework, plugin registry and `click`
/// CLI was reported as dead. The row is *tagged*, not suppressed — suppressing
/// anything decorated would hide real dead code, which is the failure `unused`
/// exists to avoid, whereas `decorator=@app.get` lets a reader discount it in one
/// glance. Same call codeq makes for `on=` receivers: show it, don't filter on it.
///
/// The inert list is the small half of the judgement. `@property`,
/// `@staticmethod`, `#[derive(Debug)]` and friends change how a definition
/// behaves; they do not hand it to anything. Tagging those would put a
/// meaningless annotation on most rows and cost the signal this adds.
fn registering_decorator(node: Node, src: &[u8]) -> Option<String> {
    // Whole namespaces that only ever change how a definition behaves. Nothing
    // in `functools`, `contextlib`, `typing`, `abc`, `dataclasses` or `enum`
    // hands a definition to a framework, so the module answers for all of them.
    const INERT_MODULES: &[&str] = &[
        "functools",
        "contextlib",
        "typing",
        "abc",
        "dataclasses",
        "enum",
        "serde",
    ];
    const INERT: &[&str] = &[
        "property",
        "setter",
        "getter",
        "deleter",
        "staticmethod",
        "classmethod",
        "abstractmethod",
        "cached_property",
        "dataclass",
        "overload",
        "override",
        "wraps",
        "final",
        // Imported bare as often as they are qualified — `from functools import
        // cache` — so the module list alone would miss them.
        "cache",
        "lru_cache",
        "singledispatch",
        "singledispatchmethod",
        "total_ordering",
        "contextmanager",
        "asynccontextmanager",
        "runtime_checkable",
        "no_type_check",
        "unique",
        "LocalProxy",
        "functools",
        "derive",
        "inline",
        "repr",
        "allow",
        "deny",
        "warn",
        "cfg",
        "must_use",
        "non_exhaustive",
        "doc",
        "Serializable",
        "Override",
        "Deprecated",
        "NotNull",
        "Nullable",
        "SuppressWarnings",
        "Obsolete",
    ];
    attributes_of(node, src).into_iter().find(|a| {
        // The bare name, minus the syntax each language wraps it in, so
        // `@app.get("/x")`, `#[derive(Debug)]` and `[Fact]` are all comparable.
        let head = a
            .trim_start_matches(['@', '#', '[', ']', ' '])
            .split(['(', '[', ' ', '=', ':'])
            .next()
            .unwrap_or("");
        // `@functools.cache` is neither `functools` nor `cache` as written, so
        // the dotted name has to be judged by its parts: the module it came
        // from and the name it binds.
        let module = head.split('.').next().unwrap_or(head);
        let last = head.rsplit('.').next().unwrap_or(head);
        !head.is_empty()
            && !INERT.contains(&head)
            && !INERT.contains(&last)
            && !INERT_MODULES.contains(&module)
    })
}

/// Does this attribute text name a test? A fixed per-language list rather than a
/// pattern, because "contains the word test" would catch `#[cfg(test)]` on a
/// module (which marks a *container*, already handled) and `@TestOnly` (which
/// marks production code). Deliberately covers the setup/teardown annotations
/// too: those are test scaffolding, which is what `unused` and `untested` need to
/// know, and they share this predicate with `tests`.
fn is_test_marker(lang: &str, attrs: &str) -> bool {
    match lang {
        // `#[test]`, `#[bench]`, and the framework forms — including any path
        // ending in `::test`, which covers `#[tokio::test]`, `#[async_std::test]`
        // and every other runtime's wrapper without listing them.
        "rust" => {
            attrs.contains("[test]")
                || attrs.contains("::test]")
                || attrs.contains("[bench]")
                || [
                    "rstest",
                    "proptest",
                    "quickcheck",
                    "test_case",
                    "googletest",
                ]
                .iter()
                .any(|m| attrs.contains(m))
        }
        // `@pytest.mark.…`, `@pytest.fixture`, `@unittest.…`, bare `@parametrize`.
        "python" => [
            "pytest.",
            "unittest",
            "parametrize",
            "@fixture",
            "hypothesis",
        ]
        .iter()
        .any(|m| attrs.contains(m)),
        "java" | "kotlin" | "scala" | "csharp" | "php" => [
            "@Test",
            "@ParameterizedTest",
            "@RepeatedTest",
            "@TestFactory",
            "@BeforeEach",
            "@AfterEach",
            "@BeforeAll",
            "@AfterAll",
            "@Before",
            "@After",
            "[Test]",
            "[TestMethod]",
            "[Fact]",
            "[Theory]",
            "[SetUp]",
            "[TearDown]",
            "[TestCase]",
            "#[Test]",
        ]
        .iter()
        .any(|m| attrs.contains(m)),
        _ => false,
    }
}

/// The name of the type enclosing a member, for `field` and `variant` facts.
/// Walks up to the nearest type declaration and reads its name — the identifier
/// child, whatever the grammar happens to call it. `None` for non-members, and
/// for a member whose owner is anonymous (a bare `struct { … }` in C).
fn owner_of(lang: &str, kind: &str, node: Node, src: &[u8]) -> Option<String> {
    if kind != "field" && kind != "variant" {
        return None;
    }
    let mut cur = node.parent();
    while let Some(n) = cur {
        if TYPE_KINDS.contains(&n.kind()) {
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

/// For a value that wraps something — `memo(…)`, `styled.div\`…\`` — the node
/// where the wrapped part starts, so a signature can be cut there and keep the
/// wrapper. `None` when the value is not a wrapper (a bare object or array
/// literal), in which case the caller cuts at the value itself.
fn wrapper_head(val: Node) -> Option<Node> {
    match val.kind() {
        "call_expression" => val.child_by_field_name("arguments"),
        "tagged_template_expression" => val.child_by_field_name("quasi"),
        _ => None,
    }
}

fn signature_of(src: &[u8], def: Node, body: Option<Node>) -> String {
    if let Some(sig) = arrow_signature(src, def) {
        return cap(&sig, MAX_SIG);
    }
    let end = match body {
        Some(b) => b.start_byte(),
        None => def.end_byte(),
    };
    let raw = &src[def.start_byte()..end.min(src.len())];
    cap(&collapse_ws(&String::from_utf8_lossy(raw)), MAX_SIG)
}

/// The callable signature of an arrow function bound to a name, or `None` when
/// `def` is not one.
///
/// The definition node for `const withCDN = (path: string) => {…}` is the
/// declarator, so taking the source up to the body yields `withCDN = (path:
/// string) =>` — an assignment fragment ending in a dangling arrow, where every
/// other definition in the same outline reads as a signature. Arrow functions
/// are how most JS/TS code declares a function, so this was the common case
/// rendering worse than the rare one. Composed from the parameter and
/// return-type fields instead, which also puts the parentheses back on the
/// unparenthesized single-parameter form (`const f = x => x`).
fn arrow_signature(src: &[u8], def: Node) -> Option<String> {
    let (name, value) = match def.kind() {
        "variable_declarator" => ("name", "value"),
        "assignment_expression" => ("left", "right"),
        _ => return None,
    };
    let arrow = def.child_by_field_name(value)?;
    if arrow.kind() != "arrow_function" {
        return None;
    }
    let text = |n: Node| collapse_ws(&String::from_utf8_lossy(&src[n.start_byte()..n.end_byte()]));
    let params = match arrow.child_by_field_name("parameters") {
        Some(p) => text(p),
        // A lone parameter needs no parentheses in source; a signature does.
        None => format!("({})", text(arrow.child_by_field_name("parameter")?)),
    };
    // `async` is a bare token with no field, and it changes what the caller gets
    // back, so it belongs in the signature.
    let asynchronous = arrow
        .children(&mut arrow.walk())
        .any(|c| c.kind() == "async");
    let generics = arrow
        .child_by_field_name("type_parameters")
        .map(text)
        .unwrap_or_default();
    let returns = arrow
        .child_by_field_name("return_type")
        .map(text)
        .unwrap_or_default();
    Some(format!(
        "{}{}{generics}{params}{returns}",
        if asynchronous { "async " } else { "" },
        text(def.child_by_field_name(name)?),
    ))
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


/// JavaScript definitions and ES imports. Node types here also exist in the
/// TypeScript grammar, so this compiles against JS, TS, and TSX. `name: (_)`
/// avoids depending on grammar-specific identifier node types.

/// JavaScript-only definitions, appended to `JS_QUERY` for .js/.jsx.
///
/// A class field is `field_definition` in the JavaScript grammar but
/// `public_field_definition` in TypeScript's, so it cannot live in the shared
/// query: a node type absent from a grammar fails the whole query to compile.

/// TypeScript-only definitions, appended to `JS_QUERY` for TS and TSX.

/// Go definitions and imports.


/// Java definitions and imports.


/// Ruby definitions and `require`/`require_relative` imports. Ruby has no import
/// statement — `require "x"` is a plain method call — so imports are captured as
/// calls guarded by the method name (see the `@guard` handling in `extract`).
/// Ruby method/class bodies aren't a strippable field here, so signatures are
/// the whole declaration, capped.


/// C definitions and #include directives. Declarator nesting means some forms
/// (e.g. pointer-returning functions) need their own pattern.


/// C++ (a superset of C): add classes, namespaces, and member functions.


/// C#.


/// PHP (the `<?php` variant).


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




// ---------------------------------------------------------------------------
// Type-relation queries: `@sub` is the implementing/deriving type, `@implements`
// / `@extends` the trait/interface/base. Go (structural typing) and C (no
// inheritance) have none. Grammar-specific; kept minimal and verified by tests.
// ---------------------------------------------------------------------------





const GO_REL: &str = "";



const C_REL: &str = "";




// Bash: functions only (source/. includes are plain commands, not captured).

// Lua: named functions, incl. `M.f` (@function) and `obj:m` (@method). `require`
// is a plain call, so imports aren't captured.

// Scala: defs, classes/objects/traits/enums, type aliases, and imports.


// Kotlin: classes/objects/functions and imports. The grammar exposes no body
// field, so signatures are the (capped) declaration, like Ruby.


// Swift: functions, class/struct/enum/actor (all `class_declaration`), protocols,
// and imports. `name:` is pinned to the identifier node because a
// `function_declaration` also exposes its return type under a `name` field.


// Dart: top-level functions, methods, classes/mixins/enums, and imports.


// Zig: functions, and the `const X = struct/enum/union {…}` type idiom.

// Solidity: contracts/interfaces/libraries, functions, modifiers, structs, enums,
// events, and imports.


// Elixir: modules and functions. These are macro `call`s (`defmodule`/`def`/
// `defp`), so the `def*` keyword is captured as a `@guard` that `extract` filters
// on (an ordinary call like `import Foo` is rejected). `defmodule` names an
// `alias`; `def`/`defp` name the inner call's (or bare) identifier.

// Svelte markup. Everything a `<script>` block declares comes from the nested
// JS/TS pass in `extract_svelte`; what the *markup* itself defines is a
// `{#snippet name(…)}` — Svelte 5's reusable fragment, invoked as
// `{@render name(…)}`, so the (textual) call graph links the two. The grammar
// exposes no body field, so a snippet's signature is its whole declaration
// capped, like Ruby's and Kotlin's.
// Markup comments and literal attribute values. `<script>`/`<style>` contents
// are each a single opaque token here: script spans come from the nested JS/TS
// pass, while CSS in `<style>` stays unclassified (no CSS grammar), so a comment
// inside `<style>` reads as code to `search --in`.

// Vue single-file components. Unlike Svelte's `{#snippet}`, a Vue `<template>`
// declares nothing nameable — every definition comes from the nested `<script>`
// pass (both `<script>` and `<script setup>`, which is just a second block) and
// from the file's own `component` fact. Hence an empty definition query.
const VUE_QUERY: &str = "";
// Markup comments and literal attribute values. The `(attribute …)` scope is
// load-bearing and the one real divergence from Svelte: Vue puts JavaScript in
// *quoted* directive values (`@click="handleClick"`, `:prop="expr"`, `v-if="c"`),
// which the grammar models as a `directive_attribute` with its own
// `quoted_attribute_value`. Matching that bare would classify template
// expressions as strings, and `callers`/`callees` — which read code only —
// would go blind to every handler a template binds. Svelte needs no such scope
// because it spells expressions with braces (`on:click={handleClick}`), which
// are never quoted values. `{{ interpolation }}` holds code and stays
// unclassified; `<style>` is opaque, as in Svelte.

// CSS. Rules are not "definitions" in the usual sense, but "where is `.primary`
// styled?" is a real question with an exact answer, so a class/id selector and a
// named at-rule are captured. The bigger win is spans: `<style>` inside a Svelte
// or Vue component was previously opaque, so CSS comments read as code.

// PowerShell: functions and classes, the Bash counterpart codeq was missing.

// Protobuf: the interface between services in a polyglot repo, so a message name
// is often the one symbol that crosses language boundaries.

// GraphQL: schema types and their fields.
// `type User implements Node & Timestamped`. The grammar nests
// `implements_interfaces` left-recursively rather than listing siblings, so
// `A & B & C` is `II(II(II(A), B), C)` and a single pattern binds only the
// outermost. One pattern per nesting level therefore, which covers up to five
// interfaces on a type — a stated bound rather than a silent truncation, and
// past anything real schemas do.


// SQL. A schema is the one part of a polyglot repo that every language talks to,
// and `.sql` was indexed as searchable text with no definitions at all — so
// `definitions users` missed the table that half the codebase reads.
//
// The anchors are load-bearing. `create_trigger` has *three* `object_reference`
// children (the trigger, the table it fires on, the function it executes) and
// `column_definition` has two identifiers when the column carries a `REFERENCES`
// clause, so an unanchored pattern names each definition several times over. `.`
// after the introducing keyword (or at the start of the child list) pins the
// capture to the one that is the declaration's own name.

// `marginalia` is this grammar's name for a `/* … */` block comment; `comment` is
// the `--` line form. String spans are *not* here: the grammar gives every scalar
// the same `literal` node, so a query cannot tell `'admin'` from `42` — see
// `sql_extras`.

// Foreign keys — the one genuinely structural edge a schema has, and the reason
// to index SQL beyond finding declarations. Deliberately *not* `implements` or
// `extends`: a foreign key is not inheritance, and forcing it into that
// vocabulary would make `supertypes` claim an is-a relationship that does not
// exist. It gets its own relation kind, `references`, so the two commands read
// directionally — `implementors orgs --relation references` is what points at
// `orgs`, `supertypes orders --relation references` is what `orders` points at.
//
// Three syntactic homes for the same edge, all of which real migrations use: an
// inline column `REFERENCES`, a table-level `CONSTRAINT … FOREIGN KEY` inside
// the `CREATE`, and an `ALTER TABLE … ADD CONSTRAINT` afterwards. Missing the
// third would lose exactly the foreign keys a migrations directory adds later,
// which is the same trap `ALTER TABLE … ADD COLUMN` set for members.

/// Remove delimiter syntax from a SQL identifier while preserving its spelling.
/// The Sequel grammar exposes quoted identifiers with their delimiters, and its
/// T-SQL bracket form can expose only the contents as the name node.
fn unquote_sql_identifier(name: &str) -> String {
    let Some((open, close)) = name
        .as_bytes()
        .first()
        .copied()
        .zip(name.as_bytes().last().copied())
        .filter(|(open, close)| {
            (*open == b'"' && *close == b'"')
                || (*open == b'`' && *close == b'`')
                || (*open == b'[' && *close == b']')
        })
    else {
        return name.to_string();
    };
    let inner = &name[1..name.len() - 1];
    let escaped = match (open, close) {
        (b'"', b'"') => "\"\"",
        (b'`', b'`') => "``",
        (b'[', b']') => "]]",
        _ => unreachable!(),
    };
    let delimiter = close as char;
    inner.replace(escaped, &delimiter.to_string())
}

/// Make the SQL grammar accept common escaped identifier delimiters without
/// changing source offsets. Quoted string literals and comments are skipped.
fn repair_sql_identifier_escapes(src: &[u8]) -> Option<Vec<u8>> {
    let mut out = src.to_vec();
    let mut changed = false;
    let mut i = 0;
    while i < src.len() {
        if src.get(i..i + 2) == Some(b"--") {
            i += 2;
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if src.get(i..i + 2) == Some(b"/*") {
            i += 2;
            while i + 1 < src.len() && src.get(i..i + 2) != Some(b"*/") {
                i += 1;
            }
            i = (i + 2).min(src.len());
            continue;
        }
        if src[i] == b'\'' {
            i += 1;
            while i < src.len() {
                if src[i] == b'\'' {
                    if src.get(i + 1) == Some(&b'\'') {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if matches!(src[i], b'"' | b'`') {
            let delimiter = src[i];
            i += 1;
            while i < src.len() {
                if src[i] == delimiter {
                    if src.get(i + 1) == Some(&delimiter) {
                        out[i] = b'_';
                        out[i + 1] = b'_';
                        changed = true;
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if src[i] == b'[' && sql_column_bracket_start(src, i) {
            let mut end = i + 1;
            let mut closed = false;
            if src
                .get(end)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            {
                while end < src.len() {
                    if src[end] == b']' {
                        if src.get(end + 1) == Some(&b']') {
                            out[end] = b'_';
                            out[end + 1] = b'_';
                            changed = true;
                            end += 2;
                        } else {
                            out[i] = b'"';
                            out[end] = b'"';
                            changed = true;
                            i = end + 1;
                            closed = true;
                            break;
                        }
                    } else {
                        end += 1;
                    }
                }
                if !closed {
                    // No closing bracket was found; leave the text untouched.
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
    changed.then_some(out)
}

fn sql_column_bracket_start(src: &[u8], start: usize) -> bool {
    src[..start]
        .iter()
        .rev()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| matches!(b, b'(' | b','))
}

/// The two SQL facts a tree-sitter query cannot express, in one walk.
///
/// **Enum variants.** `CREATE TYPE mood AS ENUM ('sad', 'ok')` declares its
/// members as quoted literals, and a query captures the quotes with them —
/// `members mood` would answer `'sad'`, which no query for `sad` ever finds.
///
/// **String spans.** This grammar gives every scalar the same `literal` node, so
/// `'admin'`, `42` and `NULL` are indistinguishable by kind. Marking them all as
/// strings would push numeric and boolean literals out of `--in code`, so the
/// quote character decides: `'…'` and `$$…$$` are strings, everything else is
/// code. That keeps `search --in code email` off `'email@example.com'` without
/// hiding a threshold or a flag value.
fn sql_extras(root: Node, src: &[u8]) -> (Vec<Fact>, Vec<(usize, usize, Span)>) {
    let mut facts = Vec::new();
    let mut spans = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
        match node.kind() {
            "literal" => {
                let text = node.utf8_text(src).unwrap_or("");
                if text.starts_with('\'') || text.starts_with('$') {
                    spans.push((node.start_byte(), node.end_byte(), Span::Str));
                }
            }
            "enum_elements" => {
                let Some(owner) = node
                    .parent()
                    .and_then(|p| first_child_kind(p, "object_reference", src))
                    .map(unqualify)
                else {
                    continue;
                };
                let mut ec = node.walk();
                for el in node.children(&mut ec) {
                    if el.kind() != "literal" {
                        continue;
                    }
                    let Ok(raw) = el.utf8_text(src) else { continue };
                    let name = raw.trim_matches('\'').to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let start = el.start_position();
                    let end = el.end_position();
                    facts.push(Fact {
                        kind: "variant".to_string(),
                        is_derived: false,
                        name,
                        start_byte: el.start_byte(),
                        end_byte: el.end_byte(),
                        start_line: start.row + 1,
                        start_col: start.column,
                        end_line: end.row + 1,
                        end_col: end.column,
                        signature: collapse_ws(raw),
                        doc: None,
                        source: "treesitter/sql".to_string(),
                        confidence: 1.0,
                        visibility: Some("public".to_string()),
                        owner: Some(owner.clone()),
                        shape: None,
                        shape_nodes: 0,
                        is_test: false,
                        decorator: None,
                        contract: None,
                    });
                }
            }
            _ => {}
        }
    }
    facts.sort_by_key(|f| f.start_byte);
    (facts, spans)
}

/// The last segment of a schema-qualified SQL name: `analytics.events` → `events`.
///
/// A definition is stored under the name it is *looked up* by. Queries write the
/// table both ways depending on `search_path`, and codeq's own reference matching
/// is textual and word-bounded — so `references events` already finds
/// `analytics.events`, while a fact stored as `analytics.events` would not answer
/// `definitions events`. The qualified form is not lost: it is what the
/// signature shows.
fn unqualify(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

// HCL / Terraform. Blocks are `keyword "label"… { … }` and their identity lives
// in the labels, not in any name node — so a query cannot pick it out: the labels
// are a *repeated, unnamed* child, and which one is the name depends on how many
// there are. `variable "region"` is named by its only label; `resource
// "aws_s3_bucket" "logs"` is named by its last and typed by its first. Hence the
// derivation below rather than a query, as with Svelte's component fact.

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
fn hcl_facts(root: Node, src: &[u8]) -> Vec<Fact> {
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

/// Minimum nodes before a shape is recorded at all. Below this every trivial
/// definition in a repository collides — truthfully, but uselessly — so the
/// cheapest place to stop is at extraction.
const MIN_SHAPE_NODES: usize = 12;

/// A definition's syntax shape: the tree of node *kinds*, with identifiers and
/// literals normalised away, hashed. Two definitions sharing a shape are the same
/// code up to renaming.
///
/// This deliberately answers a yes/no question rather than producing a similarity
/// score. A score would need a threshold nobody can justify, and "0.87" tells an
/// agent nothing it can act on — whereas "these four functions are structurally
/// identical" is checkable, and `diff` covers the rest. Walked with an explicit
/// stack: recursion here is bounded by source nesting, which is attacker-controlled.
fn shape_of(node: Node) -> (Option<String>, i64) {
    let mut h = blake3::Hasher::new();
    let mut count = 0usize;
    // (node, entered) — the close marker is what makes sibling structure matter,
    // so `f(g(x))` and `f(g, x)` cannot collide.
    let mut stack: Vec<(Node, bool)> = vec![(node, false)];
    while let Some((n, entered)) = stack.pop() {
        if entered {
            h.update(b")");
            continue;
        }
        h.update(&n.kind_id().to_le_bytes());
        h.update(b"(");
        count += 1;
        stack.push((n, true));
        // Anonymous children include punctuation and operators. Keep them in the
        // shape: dropping the operator tokens makes `a != b` and `a == b`
        // indistinguishable even though the two expressions do different work.
        let mut c = n.walk();
        let kids: Vec<Node> = n.children(&mut c).collect();
        for child in kids.into_iter().rev() {
            stack.push((child, false));
        }
    }
    if count < MIN_SHAPE_NODES {
        return (None, count as i64);
    }
    (Some(h.finalize().to_hex()[..16].to_string()), count as i64)
}
