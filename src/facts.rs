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
    fn extract(&mut self, src: &[u8]) -> Extraction {
        let tree = match self.parser.parse(src, None) {
            Some(t) => t,
            None => return (Vec::new(), Vec::new(), Vec::new()),
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
                            });
                        }
                    }
                }
                if let Some(g) = guard {
                    // A guarded match is only valid when the token is a known
                    // keyword. Elixir definitions are macro calls (`def`/`defp`/
                    // `defmodule`); every other language guards call-style imports.
                    let ok = if self.lang == "elixir" {
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
        })
    }

    /// One parse per file yields definition facts and comment/string spans.
    /// Unsupported languages yield nothing (not an error).
    pub fn extract(
        &mut self,
        language: &str,
        src: &[u8],
    ) -> Extraction {
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
            "markdown" => markdown_facts(src),
            _ => (Vec::new(), Vec::new(), Vec::new()),
        }
    }
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
                    heads.push(Head { line: i, level, name, start: *start });
                }
                continue;
            }
        }

        // Setext underline: `===` or `---` directly under paragraph text. The
        // preceding line must be ordinary prose, otherwise `---` is a thematic
        // break (or a list marker), not a heading.
        if indent < 4 && !t.is_empty() && (t.chars().all(|c| c == '=') || t.chars().all(|c| c == '-'))
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
        let end = if j + 1 < lines.len() { lines[j + 1].0 } else { eof };
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
        i = if j < bytes.len() { j + ticks } else { bytes.len() };
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

/// Every language codeq extracts from.
pub const LANGUAGES: &[&str] = &[
    "rust", "python", "javascript", "typescript", "tsx", "go", "java", "ruby",
    "c", "cpp", "csharp", "php", "bash", "lua", "scala", "kotlin", "swift",
    "dart", "zig", "elixir", "solidity", "markdown",
];

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
        "scala" => pair(SCALA_QUERY, ""),
        "kotlin" => pair(KOTLIN_QUERY, ""),
        "swift" => pair(SWIFT_QUERY, ""),
        "dart" => pair(DART_QUERY, ""),
        "zig" => pair(ZIG_QUERY, ""),
        "elixir" => pair(ELIXIR_QUERY, ""),
        "solidity" => pair(SOLIDITY_QUERY, ""),
        _ => None,
    }
}

pub fn support(language: &str) -> Option<Support> {
    // Markdown is extracted by hand, not by a query, so it is described directly.
    if language == "markdown" {
        return Some(Support {
            language: language.to_string(),
            definitions: true, // headings
            imports: true,     // local links and images
            members: false,
            relations: false,
            spans: true, // fenced code vs prose
            calls: false,
        });
    }
    let (query, rel) = queries_for(language)?;
    Some(Support {
        language: language.to_string(),
        definitions: query.contains(") @") || query.contains("@function"),
        imports: query.contains("@import"),
        members: query.contains("@field") || query.contains("@variant"),
        relations: !rel.trim().is_empty(),
        spans: has_spans(language),
        calls: crate::astgrep::ts_language(language).is_some(),
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
            | "markdown"
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
            if def.kind() == "method_definition" {
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

; Raw syntactic imports: name is the module path as written in source.
(use_declaration argument: (_) @name) @import
; An external `mod foo;` (no inline body) pulls in another file's module — a
; file-level dependency edge, so treat it as an import for reachability. Inline
; `mod foo { … }` (has a body) is not a separate file, so exclude it via `!body`.
(mod_item name: (identifier) @name !body) @import
; Members: struct/union fields and enum variants. Contained in their parent's
; byte range, which is how `members` and `outline` recover the relationship.
(field_declaration name: (field_identifier) @name) @field
(enum_variant name: (identifier) @name) @variant
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
; Class-level attributes: annotated (`a: int = 1`) or plain (`b = 2`). Scoped to
; a class body so ordinary local assignments aren't members. Instance attributes
; assigned in `__init__` (`self.x = …`) are not declarations and are not covered.
(class_definition body: (block (expression_statement
  (assignment left: (identifier) @name) @field)))
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

/// JavaScript-only definitions, appended to `JS_QUERY` for .js/.jsx.
///
/// A class field is `field_definition` in the JavaScript grammar but
/// `public_field_definition` in TypeScript's, so it cannot live in the shared
/// query: a node type absent from a grammar fails the whole query to compile.
const JS_EXTRA: &str = r#"
(field_definition property: (property_identifier) @name) @field
(field_definition property: (private_property_identifier) @name) @field
"#;

/// TypeScript-only definitions, appended to `JS_QUERY` for TS and TSX.
const TS_EXTRA: &str = r#"
(interface_declaration name: (_) @name) @interface
(type_alias_declaration name: (_) @name) @type
(enum_declaration name: (_) @name) @enum
; Typed class fields, interface properties, and enum members.
(public_field_definition name: (property_identifier) @name) @field
(property_signature name: (property_identifier) @name) @field
(enum_assignment name: (property_identifier) @name) @variant
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
; Struct fields. Embedded fields (no name) carry no identifier, so are skipped.
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
; Fields and enum constants.
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
; Struct/union fields, including pointer declarators (`char *b`).
(field_declaration declarator: (field_identifier) @name) @field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @field
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
(field_declaration declarator: (field_identifier) @name) @field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @field
(enumerator name: (identifier) @name) @variant
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
; Fields, auto/expression properties, and enum members.
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
; File inclusion (require/include and their _once variants) with a static string
; path — PHP's other import mechanism. Both single-quoted (string) and
; double-quoted (encapsed_string) forms; quotes stripped like other imports.
(require_expression [(string) (encapsed_string)] @name) @import
(require_once_expression [(string) (encapsed_string)] @name) @import
(include_expression [(string) (encapsed_string)] @name) @import
(include_once_expression [(string) (encapsed_string)] @name) @import
; Typed and untyped properties, and enum cases.
(property_declaration (property_element (variable_name (name) @name))) @field
(enum_case name: (name) @name) @variant
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
; `val`/`var` members and case-class parameters (which are fields).
(val_definition pattern: (identifier) @name) @field
(var_definition pattern: (identifier) @name) @field
(class_parameter name: (identifier) @name) @field
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
; `val`/`var` properties and primary-constructor parameters.
(property_declaration (variable_declaration (identifier) @name)) @field
(class_parameter (identifier) @name) @field
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
; Stored properties (`var`/`let` in a type body).
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
; Instance/class fields.
(declaration (initialized_identifier_list (initialized_identifier (identifier) @name))) @field
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
; Struct/union/enum members are all `container_field`.
(container_field (identifier) @name) @field
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
; Contract state variables, and enum values.
(state_variable_declaration name: (identifier) @name) @field
; A Solidity `enum_value` is itself the identifier leaf, not a wrapper, so the
; name and the definition are the same node.
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
