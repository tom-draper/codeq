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
