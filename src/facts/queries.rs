//! Tree-sitter queries, one `.scm` file per language under `queries/` at the
//! crate root, embedded at build time.
//!
//! Each language has up to three, named by what they extract:
//!
//! - `<lang>.scm`: definitions and imports. Each pattern binds `@name` (the
//!   identifier), one `@<kind>` capture on the whole definition node, and
//!   optionally `@body` (stripped from signatures).
//! - `<lang>.spans.scm`: comment and string spans, for `search --in`.
//! - `<lang>.rel.scm`: type relations. `@sub` is the implementing/deriving type,
//!   `@implements` / `@extends` the trait/interface/base. Go (structural typing)
//!   and C (no inheritance) have none. Grammar-specific; kept minimal and
//!   verified by tests.
//!
//! `.extra.scm` is appended to the shared `js.scm` for one dialect.
//!
//! An empty string means the language has none of that kind. A node type absent
//! from a grammar fails the whole query to compile, so a query is only ever
//! paired with grammars that have every node it names.

pub(super) const RUST_QUERY: &str = include_str!("../../queries/rust.scm");
pub(super) const RUST_SPANS: &str = include_str!("../../queries/rust.spans.scm");
pub(super) const RUST_REL: &str = include_str!("../../queries/rust.rel.scm");

pub(super) const PYTHON_QUERY: &str = include_str!("../../queries/python.scm");
pub(super) const PYTHON_SPANS: &str = include_str!("../../queries/python.spans.scm");
pub(super) const PYTHON_REL: &str = include_str!("../../queries/python.rel.scm");

pub(super) const JS_QUERY: &str = include_str!("../../queries/js.scm");
pub(super) const JS_EXTRA: &str = include_str!("../../queries/js.extra.scm");
pub(super) const JS_SPANS: &str = include_str!("../../queries/js.spans.scm");
pub(super) const JS_REL: &str = include_str!("../../queries/js.rel.scm");

pub(super) const TS_EXTRA: &str = include_str!("../../queries/ts.extra.scm");
pub(super) const TS_REL: &str = include_str!("../../queries/ts.rel.scm");

pub(super) const GO_QUERY: &str = include_str!("../../queries/go.scm");
pub(super) const GO_SPANS: &str = include_str!("../../queries/go.spans.scm");
// Go (structural typing) and C (no inheritance) have no type-relation query.
pub(super) const GO_REL: &str = "";

pub(super) const JAVA_QUERY: &str = include_str!("../../queries/java.scm");
pub(super) const JAVA_SPANS: &str = include_str!("../../queries/java.spans.scm");
pub(super) const JAVA_REL: &str = include_str!("../../queries/java.rel.scm");

pub(super) const RUBY_QUERY: &str = include_str!("../../queries/ruby.scm");
pub(super) const RUBY_SPANS: &str = include_str!("../../queries/ruby.spans.scm");
pub(super) const RUBY_REL: &str = include_str!("../../queries/ruby.rel.scm");

pub(super) const C_QUERY: &str = include_str!("../../queries/c.scm");
pub(super) const C_SPANS: &str = include_str!("../../queries/c.spans.scm");
pub(super) const C_REL: &str = "";

pub(super) const CPP_QUERY: &str = include_str!("../../queries/cpp.scm");
pub(super) const CPP_SPANS: &str = include_str!("../../queries/cpp.spans.scm");
pub(super) const CPP_REL: &str = include_str!("../../queries/cpp.rel.scm");

pub(super) const CSHARP_QUERY: &str = include_str!("../../queries/csharp.scm");
pub(super) const CSHARP_SPANS: &str = include_str!("../../queries/csharp.spans.scm");
pub(super) const CSHARP_REL: &str = include_str!("../../queries/csharp.rel.scm");

pub(super) const PHP_QUERY: &str = include_str!("../../queries/php.scm");
pub(super) const PHP_SPANS: &str = include_str!("../../queries/php.spans.scm");
pub(super) const PHP_REL: &str = include_str!("../../queries/php.rel.scm");

pub(super) const BASH_QUERY: &str = include_str!("../../queries/bash.scm");
pub(super) const BASH_SPANS: &str = include_str!("../../queries/bash.spans.scm");

pub(super) const LUA_QUERY: &str = include_str!("../../queries/lua.scm");
pub(super) const LUA_SPANS: &str = include_str!("../../queries/lua.spans.scm");

pub(super) const SCALA_QUERY: &str = include_str!("../../queries/scala.scm");
pub(super) const SCALA_SPANS: &str = include_str!("../../queries/scala.spans.scm");
pub(super) const SCALA_REL: &str = include_str!("../../queries/scala.rel.scm");

pub(super) const KOTLIN_QUERY: &str = include_str!("../../queries/kotlin.scm");
pub(super) const KOTLIN_SPANS: &str = include_str!("../../queries/kotlin.spans.scm");
pub(super) const KOTLIN_REL: &str = include_str!("../../queries/kotlin.rel.scm");

pub(super) const SWIFT_QUERY: &str = include_str!("../../queries/swift.scm");
pub(super) const SWIFT_SPANS: &str = include_str!("../../queries/swift.spans.scm");
pub(super) const SWIFT_REL: &str = include_str!("../../queries/swift.rel.scm");

pub(super) const DART_QUERY: &str = include_str!("../../queries/dart.scm");
pub(super) const DART_SPANS: &str = include_str!("../../queries/dart.spans.scm");
pub(super) const DART_REL: &str = include_str!("../../queries/dart.rel.scm");

pub(super) const ZIG_QUERY: &str = include_str!("../../queries/zig.scm");
pub(super) const ZIG_SPANS: &str = include_str!("../../queries/zig.spans.scm");

pub(super) const SOLIDITY_QUERY: &str = include_str!("../../queries/solidity.scm");
pub(super) const SOLIDITY_SPANS: &str = include_str!("../../queries/solidity.spans.scm");
pub(super) const SOLIDITY_REL: &str = include_str!("../../queries/solidity.rel.scm");

pub(super) const ELIXIR_QUERY: &str = include_str!("../../queries/elixir.scm");
pub(super) const ELIXIR_SPANS: &str = include_str!("../../queries/elixir.spans.scm");

pub(super) const SVELTE_QUERY: &str = include_str!("../../queries/svelte.scm");
pub(super) const SVELTE_SPANS: &str = include_str!("../../queries/svelte.spans.scm");

// Vue declares nothing nameable in its `<template>` — every definition comes from
// the nested `<script>` pass (both `<script>` and `<script setup>`) and from the
// file's own `component` fact — hence an empty definition query.
pub(super) const VUE_QUERY: &str = "";
pub(super) const VUE_SPANS: &str = include_str!("../../queries/vue.spans.scm");

pub(super) const CSS_QUERY: &str = include_str!("../../queries/css.scm");
pub(super) const CSS_SPANS: &str = include_str!("../../queries/css.spans.scm");

pub(super) const POWERSHELL_QUERY: &str = include_str!("../../queries/powershell.scm");
pub(super) const POWERSHELL_SPANS: &str = include_str!("../../queries/powershell.spans.scm");

pub(super) const PROTO_QUERY: &str = include_str!("../../queries/proto.scm");
pub(super) const PROTO_SPANS: &str = include_str!("../../queries/proto.spans.scm");

pub(super) const GRAPHQL_QUERY: &str = include_str!("../../queries/graphql.scm");
pub(super) const GRAPHQL_SPANS: &str = include_str!("../../queries/graphql.spans.scm");
pub(super) const GRAPHQL_REL: &str = include_str!("../../queries/graphql.rel.scm");

pub(super) const SQL_QUERY: &str = include_str!("../../queries/sql.scm");
pub(super) const SQL_SPANS: &str = include_str!("../../queries/sql.spans.scm");
pub(super) const SQL_REL: &str = include_str!("../../queries/sql.rel.scm");

pub(super) const HCL_SPANS: &str = include_str!("../../queries/hcl.spans.scm");

/// One language's grammar and the three queries codeq runs over it.
pub(super) struct Grammar {
    /// The language's name, as `LANGUAGES` spells it.
    pub name: &'static str,
    pub language: tree_sitter::Language,
    pub defs: String,
    pub spans: &'static str,
    pub rel: &'static str,
}

/// The grammar and queries for a language, or `None` for one extracted without
/// a parser (markdown, rst, jinja) or not extracted at all.
///
/// The one place a language is paired with its queries: the extractor compiles
/// from it and `support` reads capabilities off the same text, so the two
/// cannot describe different queries.
pub(super) fn grammar(language: &str) -> Option<Grammar> {
    let name = *super::LANGUAGES.iter().find(|l| **l == language)?;
    let (language, defs, spans, rel) = match language {
        "rust" => g(tree_sitter_rust::LANGUAGE, RUST_QUERY, RUST_SPANS, RUST_REL),
        "python" => g(
            tree_sitter_python::LANGUAGE,
            PYTHON_QUERY,
            PYTHON_SPANS,
            PYTHON_REL,
        ),
        "javascript" => g(
            tree_sitter_javascript::LANGUAGE,
            &format!("{JS_QUERY}\n{JS_EXTRA}"),
            JS_SPANS,
            JS_REL,
        ),
        "typescript" => g(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            &format!("{JS_QUERY}\n{TS_EXTRA}"),
            JS_SPANS,
            TS_REL,
        ),
        "tsx" => g(
            tree_sitter_typescript::LANGUAGE_TSX,
            &format!("{JS_QUERY}\n{TS_EXTRA}"),
            JS_SPANS,
            TS_REL,
        ),
        "go" => g(tree_sitter_go::LANGUAGE, GO_QUERY, GO_SPANS, GO_REL),
        "java" => g(tree_sitter_java::LANGUAGE, JAVA_QUERY, JAVA_SPANS, JAVA_REL),
        "ruby" => g(tree_sitter_ruby::LANGUAGE, RUBY_QUERY, RUBY_SPANS, RUBY_REL),
        "c" => g(tree_sitter_c::LANGUAGE, C_QUERY, C_SPANS, C_REL),
        "cpp" => g(tree_sitter_cpp::LANGUAGE, CPP_QUERY, CPP_SPANS, CPP_REL),
        "csharp" => g(
            tree_sitter_c_sharp::LANGUAGE,
            CSHARP_QUERY,
            CSHARP_SPANS,
            CSHARP_REL,
        ),
        "php" => g(tree_sitter_php::LANGUAGE_PHP, PHP_QUERY, PHP_SPANS, PHP_REL),
        "bash" => g(tree_sitter_bash::LANGUAGE, BASH_QUERY, BASH_SPANS, ""),
        "lua" => g(tree_sitter_lua::LANGUAGE, LUA_QUERY, LUA_SPANS, ""),
        "scala" => g(
            tree_sitter_scala::LANGUAGE,
            SCALA_QUERY,
            SCALA_SPANS,
            SCALA_REL,
        ),
        "kotlin" => g(
            tree_sitter_kotlin_ng::LANGUAGE,
            KOTLIN_QUERY,
            KOTLIN_SPANS,
            KOTLIN_REL,
        ),
        "swift" => g(
            tree_sitter_swift::LANGUAGE,
            SWIFT_QUERY,
            SWIFT_SPANS,
            SWIFT_REL,
        ),
        "dart" => g(tree_sitter_dart::LANGUAGE, DART_QUERY, DART_SPANS, DART_REL),
        "zig" => g(tree_sitter_zig::LANGUAGE, ZIG_QUERY, ZIG_SPANS, ""),
        "elixir" => g(tree_sitter_elixir::LANGUAGE, ELIXIR_QUERY, ELIXIR_SPANS, ""),
        "solidity" => g(
            tree_sitter_solidity::LANGUAGE,
            SOLIDITY_QUERY,
            SOLIDITY_SPANS,
            SOLIDITY_REL,
        ),
        "svelte" => g(
            tree_sitter_svelte_ng::LANGUAGE,
            SVELTE_QUERY,
            SVELTE_SPANS,
            "",
        ),
        "vue" => g(tree_sitter_vue_next::LANGUAGE, VUE_QUERY, VUE_SPANS, ""),
        "css" => g(tree_sitter_css::LANGUAGE, CSS_QUERY, CSS_SPANS, ""),
        "hcl" => g(tree_sitter_hcl::LANGUAGE, "", HCL_SPANS, ""),
        "powershell" => g(
            tree_sitter_powershell::LANGUAGE,
            POWERSHELL_QUERY,
            POWERSHELL_SPANS,
            "",
        ),
        "proto" => g(tree_sitter_proto::LANGUAGE, PROTO_QUERY, PROTO_SPANS, ""),
        "graphql" => g(
            tree_sitter_graphql::LANGUAGE,
            GRAPHQL_QUERY,
            GRAPHQL_SPANS,
            GRAPHQL_REL,
        ),
        "sql" => g(tree_sitter_sequel::LANGUAGE, SQL_QUERY, SQL_SPANS, SQL_REL),
        _ => return None,
    };
    Some(Grammar {
        name,
        language,
        defs,
        spans,
        rel,
    })
}

/// One row of the table in [`grammar`].
fn g(
    language: impl Into<tree_sitter::Language>,
    defs: &str,
    spans: &'static str,
    rel: &'static str,
) -> (tree_sitter::Language, String, &'static str, &'static str) {
    (language.into(), defs.to_string(), spans, rel)
}
