//! What codeq can extract per language, derived from the query text itself.
//! `capabilities` reports it, and empty-result notes cite it.

use super::queries::*;

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
