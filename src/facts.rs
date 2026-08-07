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
        let capture_names = query.capture_names().iter().map(|s| s.to_string()).collect();
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
    fn extract(&mut self, src: &[u8]) -> (Vec<Fact>, Vec<(usize, usize, Span)>, Vec<Relation>) {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return (Vec::new(), Vec::new(), Vec::new()),
        };
        self.harvest(tree.root_node(), src)
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
                // For `from MODULE import a, b` (Python): the module and each
                // imported name, combined below into qualified `MODULE.a` imports.
                let mut from_module: Option<&str> = None;
                let mut from_names: Vec<Node> = Vec::new();
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
                        "from_module" => from_module = cap.node.utf8_text(src).ok(),
                        "from_name" => from_names.push(cap.node),
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
                                name: format!("{module}.{local}"),
                                start_byte: nn.start_byte(),
                                end_byte: nn.end_byte(),
                                start_line: start.row + 1,
                                start_col: start.column,
                                end_line: end.row + 1,
                                end_col: end.column,
                                signature: String::new(),
                                doc: None,
                                source: self.source.clone(),
                                confidence: 1.0,
                                visibility: None,
                                owner: None,
                                shape: None,
                                shape_nodes: 0,
                            });
                        }
                    }
                }
                if let Some(g) = guard {
                    // A guarded match is only valid when the token is a known
                    // keyword. Elixir definitions are macro calls (`def`/`defp`/
                    // `defmodule`); every other language guards call-style imports.
                    // Python instance attributes are assignments, and only a
                    // `self.`-qualified one declares a member; `other.x = 1`
                    // mutates somebody else's object.
                    let ok = if self.lang == "python" {
                        g == "self"
                    } else if self.lang == "elixir" {
                        matches!(g, "def" | "defp" | "defmacro" | "defmacrop"
                            | "defmodule" | "defprotocol" | "defimpl" | "defguard")
                    } else {
                        matches!(g, "require" | "require_relative"
                            | "require_dependency" | "load" | "autoload")
                    };
                    if !ok {
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
                    let visibility = if kind == "import" {
                        None
                    } else {
                        visibility_of(self.lang, &name, node, src)
                    };
                    let start = node.start_position();
                    let end = node.end_position();
                    // A member with no declaring type is not addressable — an
                    // anonymous C struct's field, or a `self.x =` in a plain
                    // function — and an owner-less `field` is exactly the bare
                    // name this fact kind exists to avoid.
                    let owner = owner_of(kind, node, src);
                    let (shape, shape_nodes) = shape_of(node);
                    if matches!(kind, "field" | "variant") && owner.is_none() {
                        continue;
                    }
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
                        visibility,
                        owner,
                        shape,
                        shape_nodes,
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
                JS_QUERY,
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
                "",
                "treesitter/scala",
                "scala",
            )?,
            kotlin: LangSpec::new(
                tree_sitter_kotlin_ng::LANGUAGE.into(),
                KOTLIN_QUERY,
                KOTLIN_SPANS,
                "",
                "treesitter/kotlin",
                "kotlin",
            )?,
            swift: LangSpec::new(
                tree_sitter_swift::LANGUAGE.into(),
                SWIFT_QUERY,
                SWIFT_SPANS,
                "",
                "treesitter/swift",
                "swift",
            )?,
            dart: LangSpec::new(
                tree_sitter_dart::LANGUAGE.into(),
                DART_QUERY,
                DART_SPANS,
                "",
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
                "",
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
                CSS_QUERY, CSS_SPANS, "", "treesitter/css", "css",
            )?,
            hcl: LangSpec::new(
                tree_sitter_hcl::LANGUAGE.into(),
                "", HCL_SPANS, "", "treesitter/hcl", "hcl",
            )?,
            powershell: LangSpec::new(
                tree_sitter_powershell::LANGUAGE.into(),
                POWERSHELL_QUERY, POWERSHELL_SPANS, "", "treesitter/powershell", "powershell",
            )?,
            proto: LangSpec::new(
                tree_sitter_proto::LANGUAGE.into(),
                PROTO_QUERY, PROTO_SPANS, "", "treesitter/proto", "proto",
            )?,
            graphql: LangSpec::new(
                tree_sitter_graphql::LANGUAGE.into(),
                GRAPHQL_QUERY, GRAPHQL_SPANS, GRAPHQL_REL, "treesitter/graphql", "graphql",
            )?,
        })
    }

    /// One parse per file yields definition facts and comment/string spans.
    /// Unsupported languages yield nothing (not an error). `path` is only
    /// consulted by languages whose file *is* a definition (Svelte components).
    pub fn extract(
        &mut self,
        language: &str,
        path: &str,
        src: &[u8],
    ) -> (Vec<Fact>, Vec<(usize, usize, Span)>, Vec<Relation>) {
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
            "css" => self.css.extract(src),
            // Derived rather than queried — see `hcl_facts`.
            "hcl" => {
                let tree = match self.hcl.parser.parse(src, None) {
                    Some(t) => t,
                    None => return (Vec::new(), Vec::new(), Vec::new()),
                };
                let root = tree.root_node();
                let (mut facts, spans, rels) = self.hcl.harvest(root, src);
                facts.extend(hcl_facts(root, src));
                (facts, spans, rels)
            }
            "powershell" => self.powershell.extract(src),
            "proto" => self.proto.extract(src),
            "graphql" => self.graphql.extract(src),
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
            _ => (Vec::new(), Vec::new(), Vec::new()),
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
) -> (Vec<Fact>, Vec<(usize, usize, Span)>, Vec<Relation>) {
    let tree = match markup.parser.parse(src, None) {
        Some(t) => t,
        None => return (Vec::new(), Vec::new(), Vec::new()),
    };
    let root = tree.root_node();
    let (mut facts, mut spans, mut relations) = markup.harvest(root, src);

    for (start, end, is_ts) in script_regions(root, src) {
        let blanked = blank_outside(src, start, end);
        let (f, s, r) = if is_ts {
            typescript.extract(&blanked)
        } else {
            javascript.extract(&blanked)
        };
        facts.extend(f);
        spans.extend(s);
        relations.extend(r);
    }
    facts.extend(component_fact(path, src));
    spans.sort_by_key(|(s, _, _)| *s);
    (facts, spans, relations)
}

/// The byte range of each `<script>` block's *contents*, plus whether the block
/// declares TypeScript. Walked from the tree rather than queried so the one
/// parse in `extract_svelte` serves both passes.
fn script_regions(root: Node, src: &[u8]) -> Vec<(usize, usize, bool)> {
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

/// A copy of `src` with every byte outside `[start, end)` replaced by a space,
/// newlines excepted. Parsing this instead of the slice keeps the embedded
/// region at its original byte offsets, rows and columns, so facts extracted
/// from it are already in file coordinates.
fn blank_outside(src: &[u8], start: usize, end: usize) -> Vec<u8> {
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
    })
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
];

/// The name of the type enclosing a member, for `field` and `variant` facts.
/// Walks up to the nearest type declaration and reads its name — the identifier
/// child, whatever the grammar happens to call it. `None` for non-members, and
/// for a member whose owner is anonymous (a bare `struct { … }` in C).
fn owner_of(kind: &str, node: Node, src: &[u8]) -> Option<String> {
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
                return Some(t.to_string());
            }
            // No `name:` field, so take the first child that *is* the name.
            // `_name` catches the grammars that spell it out per construct
            // (protobuf's `message_name`, `enum_name`, …).
            let mut c = n.walk();
            let own = n
                .children(&mut c)
                .find(|ch| {
                    let k = ch.kind();
                    k.ends_with("identifier") || k == "name" || k.ends_with("_name")
                })
                .and_then(|ch| ch.utf8_text(src).ok())
                .map(|t| t.to_string());
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
                .map(|t| t.to_string());
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
        _ => None,
    }
}

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

; Members. `field` and `variant` carry an `owner` (the enclosing type), because
; member names collide constantly across types and mean little alone.
(field_declaration name: (field_identifier) @name) @field
(enum_variant name: (identifier) @name) @variant

; Raw syntactic imports: name is the module path as written in source.
(use_declaration argument: (_) @name) @import
; An external `mod foo;` (no inline body) pulls in another file's module — a
; file-level dependency edge, so treat it as an import for reachability. Inline
; `mod foo { … }` (has a body) is not a separate file, so exclude it via `!body`.
(mod_item name: (identifier) @name !body) @import
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (identifier) @name body: (block) @body) @function
(class_definition name: (identifier) @name body: (block) @body) @class

; Raw syntactic imports.
(import_statement (dotted_name) @name) @import
(import_statement (aliased_import (dotted_name) @name)) @import
; `from X import a, b [as c]` — capture the module and each imported name; the
; extractor combines them into qualified `X.a` imports so the name (which the
; bare module string drops) resolves to its defining file.
(import_from_statement
  module_name: (dotted_name) @from_module
  name: (dotted_name (identifier) @from_name))
(import_from_statement
  module_name: (dotted_name) @from_module
  name: (aliased_import name: (dotted_name (identifier) @from_name)))
; `from X import *` (wildcard) and relative `from . import x` — keep the coarse
; module string (no specific name to qualify with).
(import_from_statement module_name: (dotted_name) @name (wildcard_import)) @import
(import_from_statement module_name: (relative_import) @name) @import

; Members. Python declares attributes by assigning them, so both the class-body
; form and `self.x = …` count; the latter is guarded on its receiver so
; `other.x = 1` (mutating somebody else's object) is not a declaration.
; `@field` sits on the *assignment*, not on the class: the owner is resolved by
; walking up from the fact's own node, so capturing the class would look for the
; class's own parent and find no type at all.
(class_definition
  body: (block (expression_statement (assignment left: (identifier) @name) @field)))
(assignment left: (attribute object: (identifier) @guard attribute: (identifier) @name)) @field
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
; Named imports `import { a, b [as c] } from './x'` — qualified `./x.a` per name
; (the source name, not the alias, picks the exporting file) so the imported
; binding resolves. The coarse module capture above still covers default/namespace.
(import_statement
  (import_clause (named_imports (import_specifier name: (identifier) @from_name)))
  source: (string (string_fragment) @from_module))
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
(public_field_definition name: (_) @name) @field
(property_signature name: (_) @name) @field
(enum_assignment name: (_) @name) @variant
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
(field_declaration name: (field_identifier) @name) @field
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
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @field
(enum_constant name: (identifier) @name) @variant
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
(field_declaration declarator: (field_identifier) @name) @field
(enumerator name: (identifier) @name) @variant
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
(field_declaration (variable_declaration (variable_declarator (identifier) @name))) @field
(property_declaration name: (identifier) @name) @field
(enum_member_declaration name: (identifier) @name) @variant
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
(property_declaration (property_element (variable_name (name) @name))) @field
(enum_case name: (name) @name) @variant
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

// ---------------------------------------------------------------------------
// Type-relation queries: `@sub` is the implementing/deriving type, `@implements`
// / `@extends` the trait/interface/base. Go (structural typing) and C (no
// inheritance) have none. Grammar-specific; kept minimal and verified by tests.
// ---------------------------------------------------------------------------

const RUST_REL: &str = r#"
(impl_item trait: (_) @implements type: (_) @sub)
"#;

const PYTHON_REL: &str = r#"
(class_definition
  name: (identifier) @sub
  superclasses: (argument_list [(identifier) (attribute)] @extends))
"#;

const JS_REL: &str = r#"
(class_declaration name: (identifier) @sub (class_heritage (identifier) @extends))
(class_declaration name: (identifier) @sub (class_heritage (member_expression) @extends))
"#;

const TS_REL: &str = r#"
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (extends_clause value: (identifier) @extends)))
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (implements_clause (type_identifier) @implements)))
(interface_declaration
  name: (type_identifier) @sub
  (extends_type_clause (type_identifier) @extends))
"#;

const GO_REL: &str = "";

const JAVA_REL: &str = r#"
(class_declaration name: (identifier) @sub (superclass (type_identifier) @extends))
(class_declaration
  name: (identifier) @sub
  (super_interfaces (type_list (type_identifier) @implements)))
(interface_declaration
  name: (identifier) @sub
  (extends_interfaces (type_list (type_identifier) @extends)))
"#;

const RUBY_REL: &str = r#"
(class name: (constant) @sub superclass: (superclass (constant) @extends))
"#;

const C_REL: &str = "";

const CPP_REL: &str = r#"
(class_specifier name: (type_identifier) @sub (base_class_clause (type_identifier) @extends))
(struct_specifier name: (type_identifier) @sub (base_class_clause (type_identifier) @extends))
"#;

const CSHARP_REL: &str = r#"
(class_declaration name: (identifier) @sub (base_list (identifier) @extends))
(interface_declaration name: (identifier) @sub (base_list (identifier) @extends))
"#;

const PHP_REL: &str = r#"
(class_declaration name: (name) @sub (base_clause (name) @extends))
(class_declaration name: (name) @sub (class_interface_clause (name) @implements))
(interface_declaration name: (name) @sub (base_clause (name) @extends))
"#;

// Bash: functions only (source/. includes are plain commands, not captured).
const BASH_QUERY: &str = r#"
(function_definition name: (word) @name body: (_) @body) @function
"#;
const BASH_SPANS: &str = r#"
(comment) @comment
(string) @string
(raw_string) @string
(ansi_c_string) @string
(translated_string) @string
"#;

// Lua: named functions, incl. `M.f` (@function) and `obj:m` (@method). `require`
// is a plain call, so imports aren't captured.
const LUA_QUERY: &str = r#"
(function_declaration name: (identifier) @name body: (block) @body) @function
(function_declaration name: (dot_index_expression) @name body: (block) @body) @function
(function_declaration name: (method_index_expression) @name body: (block) @body) @method
"#;
const LUA_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;

// Scala: defs, classes/objects/traits/enums, type aliases, and imports.
const SCALA_QUERY: &str = r#"
(function_definition name: (identifier) @name body: (_) @body) @function
(function_definition name: (identifier) @name !body) @function
(function_declaration name: (identifier) @name) @function
(class_definition name: (identifier) @name body: (_) @body) @class
(class_definition name: (identifier) @name !body) @class
(object_definition name: (identifier) @name body: (_) @body) @class
(object_definition name: (identifier) @name !body) @class
(trait_definition name: (identifier) @name body: (_) @body) @trait
(trait_definition name: (identifier) @name !body) @trait
(enum_definition name: (identifier) @name) @enum
(type_definition name: (identifier) @name) @type
(import_declaration path: (_) @name) @import
(val_definition pattern: (identifier) @name) @field
(var_definition pattern: (identifier) @name) @field
(val_declaration name: (identifier) @name) @field
(var_declaration name: (identifier) @name) @field
(simple_enum_case name: (identifier) @name) @variant
"#;
const SCALA_SPANS: &str = r#"
(comment) @comment
(block_comment) @comment
(string) @string
(interpolated_string) @string
"#;

// Kotlin: classes/objects/functions and imports. The grammar exposes no body
// field, so signatures are the (capped) declaration, like Ruby.
const KOTLIN_QUERY: &str = r#"
(class_declaration name: (_) @name) @class
(object_declaration name: (_) @name) @class
(function_declaration name: (_) @name) @function
(import [(identifier) (qualified_identifier)] @name) @import
(property_declaration (variable_declaration (identifier) @name)) @field
"#;
const KOTLIN_SPANS: &str = r#"
(line_comment) @comment
(block_comment) @comment
(string_literal) @string
"#;

// Swift: functions, class/struct/enum/actor (all `class_declaration`), protocols,
// and imports. `name:` is pinned to the identifier node because a
// `function_declaration` also exposes its return type under a `name` field.
const SWIFT_QUERY: &str = r#"
(function_declaration name: (simple_identifier) @name body: (function_body) @body) @function
(function_declaration name: (simple_identifier) @name !body) @function
(protocol_function_declaration name: (simple_identifier) @name) @method
(class_declaration name: (type_identifier) @name body: (class_body) @body) @class
(class_declaration name: (type_identifier) @name !body) @class
(protocol_declaration name: (type_identifier) @name body: (protocol_body) @body) @interface
(protocol_declaration name: (type_identifier) @name !body) @interface
(import_declaration (identifier) @name) @import
(property_declaration (pattern (simple_identifier) @name)) @field
"#;
const SWIFT_SPANS: &str = r#"
(comment) @comment
(multiline_comment) @comment
(line_string_literal) @string
(multi_line_string_literal) @string
"#;

// Dart: top-level functions, methods, classes/mixins/enums, and imports.
const DART_QUERY: &str = r#"
(function_declaration signature: (function_signature name: (identifier) @name) body: (function_body) @body) @function
(method_declaration (method_signature (function_signature name: (identifier) @name)) body: (function_body) @body) @method
(class_declaration name: (identifier) @name body: (class_body) @body) @class
(mixin_declaration name: (identifier) @name body: (class_body) @body) @class
(enum_declaration name: (identifier) @name body: (_) @body) @enum
(library_import (import_specification uri: (configurable_uri (uri (string_literal) @name)))) @import
(class_member (declaration (initialized_identifier_list
  (initialized_identifier name: (identifier) @name)))) @field
(class_member (declaration (static_final_declaration_list
  (static_final_declaration name: (identifier) @name)))) @field
(enum_constant name: (identifier) @name) @variant
"#;
const DART_SPANS: &str = r#"
(comment) @comment
(string_literal) @string
"#;

// Zig: functions, and the `const X = struct/enum/union {…}` type idiom.
const ZIG_QUERY: &str = r#"
(function_declaration name: (identifier) @name body: (block) @body) @function
(variable_declaration (identifier) @name (struct_declaration)) @struct
(variable_declaration (identifier) @name (enum_declaration)) @enum
(variable_declaration (identifier) @name (union_declaration)) @union
(container_field name: (identifier) @name) @field
"#;
const ZIG_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;

// Solidity: contracts/interfaces/libraries, functions, modifiers, structs, enums,
// events, and imports.
const SOLIDITY_QUERY: &str = r#"
(contract_declaration name: (identifier) @name body: (contract_body) @body) @class
(interface_declaration name: (identifier) @name body: (contract_body) @body) @interface
(library_declaration name: (identifier) @name body: (contract_body) @body) @class
(function_definition name: (identifier) @name body: (_) @body) @function
(function_definition name: (identifier) @name !body) @function
(modifier_definition name: (identifier) @name) @function
(struct_declaration name: (identifier) @name) @struct
(enum_declaration name: (identifier) @name) @enum
(event_definition name: (identifier) @name) @type
(import_directive source: (string) @name) @import
(state_variable_declaration name: (identifier) @name) @field
(constant_variable_declaration name: (identifier) @name) @field
(struct_member name: (identifier) @name) @field
(enum_value) @name @variant
"#;
const SOLIDITY_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;

// Elixir: modules and functions. These are macro `call`s (`defmodule`/`def`/
// `defp`), so the `def*` keyword is captured as a `@guard` that `extract` filters
// on (an ordinary call like `import Foo` is rejected). `defmodule` names an
// `alias`; `def`/`defp` name the inner call's (or bare) identifier.
const ELIXIR_QUERY: &str = r#"
(call target: (identifier) @guard (arguments (alias) @name)) @module
(call target: (identifier) @guard (arguments (call target: (identifier) @name))) @function
(call target: (identifier) @guard (arguments (identifier) @name)) @function
"#;
const ELIXIR_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;

// Svelte markup. Everything a `<script>` block declares comes from the nested
// JS/TS pass in `extract_svelte`; what the *markup* itself defines is a
// `{#snippet name(…)}` — Svelte 5's reusable fragment, invoked as
// `{@render name(…)}`, so the (textual) call graph links the two. The grammar
// exposes no body field, so a snippet's signature is its whole declaration
// capped, like Ruby's and Kotlin's.
const SVELTE_QUERY: &str = r#"
(snippet_statement (snippet_start (snippet_name) @name)) @function
"#;
// Markup comments and literal attribute values. `<script>`/`<style>` contents
// are each a single opaque token here: script spans come from the nested JS/TS
// pass, while CSS in `<style>` stays unclassified (no CSS grammar), so a comment
// inside `<style>` reads as code to `search --in`.
const SVELTE_SPANS: &str = r#"
(comment) @comment
(quoted_attribute_value) @string
"#;

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
const VUE_SPANS: &str = r#"
(comment) @comment
(attribute (quoted_attribute_value) @string)
"#;

// CSS. Rules are not "definitions" in the usual sense, but "where is `.primary`
// styled?" is a real question with an exact answer, so a class/id selector and a
// named at-rule are captured. The bigger win is spans: `<style>` inside a Svelte
// or Vue component was previously opaque, so CSS comments read as code.
const CSS_QUERY: &str = r#"
(rule_set (selectors (class_selector (class_name) @name))) @type
(rule_set (selectors (id_selector (id_name) @name))) @type
(keyframes_statement (keyframes_name) @name) @type
(import_statement (string_value) @name) @import
"#;
const CSS_SPANS: &str = r#"
(comment) @comment
(string_value) @string
"#;

// PowerShell: functions and classes, the Bash counterpart codeq was missing.
const POWERSHELL_QUERY: &str = r#"
(function_statement (function_name) @name) @function
(class_statement (simple_name) @name) @class
(class_method_definition (simple_name) @name) @method
"#;
const POWERSHELL_SPANS: &str = r#"
(comment) @comment
(string_literal) @string
(expandable_string_literal) @string
(expandable_here_string_literal) @string
"#;

// Protobuf: the interface between services in a polyglot repo, so a message name
// is often the one symbol that crosses language boundaries.
const PROTO_QUERY: &str = r#"
(message (message_name) @name) @class
(enum (enum_name) @name) @enum
(service (service_name) @name) @interface
(rpc (rpc_name) @name) @method
(field (identifier) @name) @field
(enum_field (identifier) @name) @variant
(import (string) @name) @import
"#;
const PROTO_SPANS: &str = r#"
(comment) @comment
(string) @string
"#;

// GraphQL: schema types and their fields.
const GRAPHQL_QUERY: &str = r#"
(object_type_definition (name) @name) @type
(interface_type_definition (name) @name) @interface
(input_object_type_definition (name) @name) @type
(enum_type_definition (name) @name) @enum
(scalar_type_definition (name) @name) @type
(union_type_definition (name) @name) @type
(directive_definition (name) @name) @function
(field_definition (name) @name) @field
(enum_value_definition (enum_value) @name) @variant
"#;
// `type User implements Node & Timestamped`. The grammar nests
// `implements_interfaces` left-recursively rather than listing siblings, so
// `A & B & C` is `II(II(II(A), B), C)` and a single pattern binds only the
// outermost. One pattern per nesting level therefore, which covers up to five
// interfaces on a type — a stated bound rather than a silent truncation, and
// past anything real schemas do.
const GRAPHQL_REL: &str = r#"
(object_type_definition
  (name) @sub
  (implements_interfaces (named_type (name) @implements)))
(interface_type_definition
  (name) @sub
  (implements_interfaces (named_type (name) @implements)))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (named_type (name) @implements))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (named_type (name) @implements))))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements))))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements))))))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))))
"#;

const GRAPHQL_SPANS: &str = r#"
(comment) @comment
(string_value) @string
"#;

// HCL / Terraform. Blocks are `keyword "label"… { … }` and their identity lives
// in the labels, not in any name node — so a query cannot pick it out: the labels
// are a *repeated, unnamed* child, and which one is the name depends on how many
// there are. `variable "region"` is named by its only label; `resource
// "aws_s3_bucket" "logs"` is named by its last and typed by its first. Hence the
// derivation below rather than a query, as with Svelte's component fact.
const HCL_SPANS: &str = r#"
(comment) @comment
(string_lit) @string
(heredoc_template) @string
"#;

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
                if let Some(v) = first_descendant(attr, "string_lit")
                    .and_then(|l| hcl_label(l, src))
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
        let mut c = n.walk();
        let kids: Vec<Node> = n.named_children(&mut c).collect();
        for child in kids.into_iter().rev() {
            stack.push((child, false));
        }
    }
    if count < MIN_SHAPE_NODES {
        return (None, count as i64);
    }
    (Some(h.finalize().to_hex()[..16].to_string()), count as i64)
}
