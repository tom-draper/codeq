//! codeq — a deterministic single source of truth for repository intelligence,
//! built for AI agents.
//!
//! Indexes a repository into SQLite (daemonless, incremental, ignore-aware) and
//! answers structured queries over files, symbols, imports, and file/import
//! aggregates, plus token-budgeted context assembly — all as compact JSON.
//! Results are deterministic and reproducible; codeq reports facts, it does not
//! guess intent or rank by opinion.

mod astgrep;
mod callsite;
mod commands;
mod context;
mod db;
mod deps;
mod envelope;
mod facts;
mod index;
mod install;
mod mcp;
mod output;
mod repo;
mod search;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "codeq", version, about, long_about = None)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,

    /// Run against this directory as the repository root, instead of discovering
    /// one from the current directory.
    ///
    /// Takes the directory literally: it does not walk up to a Git toplevel, and
    /// it ignores the *parent* repository's ignore rules. That is what makes a
    /// dependency readable — `--root node_modules/lodash`, a vendored tree, or a
    /// checkout outside the current repo. The index is written to `.codeq`
    /// inside that directory, as it is anywhere else.
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Emit JSON (the `codeq.result.v2` envelope). Default is compact grep-style
    /// text — fewer tokens. Use this for programmatic consumers.
    #[arg(long, global = true)]
    json: bool,

    /// Include the full repo/index metadata envelope on every result.
    #[arg(long, global = true)]
    meta: bool,

    /// Pretty-print JSON for humans (implies --meta). Default is compact.
    #[arg(long, global = true)]
    pretty: bool,

    /// Keep only these fields on each data row (comma-separated).
    #[arg(long, global = true, value_delimiter = ',')]
    fields: Option<Vec<String>>,

    /// Group text rows under a `path (count)` header instead of repeating the
    /// path on every line.
    ///
    /// Off by default on purpose: the flat `path:line:` form is
    /// self-describing, so any single line stays meaningful after being piped,
    /// grepped, or quoted on its own. Grouping trades that away for fewer
    /// repeated bytes, so ask for it explicitly. Never inferred from whether
    /// stdout is a terminal — the output shape must not depend on how codeq was
    /// invoked. No effect under `--json`, which is already structured.
    #[arg(long, global = true)]
    group: bool,

    /// Keep at most N rows per file.
    ///
    /// Lossy, unlike --group, so it changes the result itself rather than only
    /// its formatting: JSON and text report the same rows. Every capped file is
    /// reported with its true count (`a.rs: +237 more (of 240)` in text,
    /// `notes.truncated_per_file` in single-name JSON). Batches report counts
    /// in each `data.results` entry's `truncated_per_file`, with a text warning
    /// per capped name, even when --fields removes the path.
    #[arg(long, global = true, value_name = "N")]
    max_per_file: Option<usize>,
}

impl Cli {
    pub(crate) fn output_options(&self) -> commands::Out {
        commands::Out {
            json: self.json,
            meta: self.meta,
            pretty: self.pretty,
            fields: self.fields.clone(),
            group: self.group,
            max_per_file: self.max_per_file,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Force a full rebuild or pre-warm the index. Rarely needed: every other
    /// command auto-refreshes changed files before answering, so you never have
    /// to run this for a query to see the current tree.
    Index {
        /// Discard the existing index and reparse every file.
        #[arg(long)]
        rebuild: bool,
        /// Reclaim unused database pages. Rewrites the index and may wait for readers.
        #[arg(long)]
        compact: bool,
    },
    /// List indexed files, optionally filtered.
    Files {
        /// Substring to match against file paths.
        pattern: Option<String>,
        /// Restrict to a single language (e.g. rust, typescript).
        #[arg(long)]
        language: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Files changed relative to a Git ref (merge-base with HEAD).
    Changed {
        /// Ref to compare against, e.g. main.
        #[arg(long, default_value = "main")]
        compare_to: String,
        /// Report the definitions the change touched, not the files.
        #[arg(long)]
        symbols: bool,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Definitions whose syntax shape is identical up to renaming — the same
    /// code written twice. Exact, not fuzzy: shapes match or they don't, so
    /// there is no similarity score to interpret. Use before writing a helper,
    /// or to find the copy-paste that needs the same fix in several places.
    Clones {
        /// Restrict to paths containing this substring.
        ///
        /// Positional, like `unused`, `untested`, `hotspots` and `tree`. It was
        /// the one command in that family that insisted on `--path`.
        path: Option<String>,
        /// Only this kind (e.g. function, method).
        #[arg(long)]
        kind: Option<String>,
        /// Minimum syntax nodes a shared shape must cover.
        #[arg(long, default_value_t = 24)]
        min_nodes: i64,
        /// Include definitions in tests.
        ///
        /// Off by default: two assertions in sequence share a shape however
        /// different the assertions are, so a test suite is a field of true
        /// positives nobody wants, and it drowns the duplication worth finding.
        #[arg(long)]
        include_tests: bool,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// For a diff: the definitions it touched, their transitive callers (the
    /// impacted set), and the tests exercising any of them — what to re-check and
    /// re-run for the change. Name-based.
    Affected {
        /// Ref to compare against, e.g. main.
        #[arg(long, default_value = "main")]
        compare_to: String,
        /// How many hops up the call tree to include as impacted callers.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Maximum test rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// BROWSE many symbols: list definitions matching a pattern/kind/path, e.g.
    /// "every struct in src/". Use `definitions` instead when you know the exact
    /// name and want to jump to it.
    Symbols {
        /// Substring to match against symbol names.
        pattern: Option<String>,
        /// Restrict to a symbol kind (e.g. function, struct, class).
        #[arg(long)]
        kind: Option<String>,
        /// Restrict to members of this type (fields/variants carry their owner).
        #[arg(long)]
        owner: Option<String>,
        /// Restrict to a language.
        #[arg(long)]
        language: Option<String>,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Include each symbol's signature (declaration minus body).
        #[arg(long)]
        signatures: bool,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// What a type contains: its fields, enum variants, and methods.
    ///
    /// Containment comes from the parse tree, not from name matching: a member is
    /// a definition nested directly inside NAME's byte range. Languages that do
    /// not declare members (Ruby `attr_accessor`, Python `self.x` in `__init__`)
    /// yield nothing rather than a guess.
    Members {
        /// The type whose members to list.
        name: String,
        /// Restrict to definitions in paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Restrict to one member kind (field, variant, method, function).
        #[arg(long)]
        kind: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// The structure of a file top to bottom: definitions, signatures, nesting.
    Outline {
        /// File to outline (repo-relative path or basename).
        file: String,
        /// Include docstrings for each definition.
        #[arg(long)]
        docs: bool,
    },
    /// Locate definitions of exact names, in argument order.
    /// Use `symbols` to browse by pattern.
    Definitions {
        /// Exact symbol names to resolve.
        #[arg(required = true, num_args = 1..)]
        symbol: Vec<String>,
        /// Restrict to definitions in paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum rows per name (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// The public API surface: definitions exported for outside use, with
    /// signatures. "Public" is each language's own notion (Rust `pub`, JS/TS
    /// `export`, Go capitalization, Python non-underscore, a `public` modifier,
    /// non-`static` in C/C++). Scope with a path; filter with `--kind`.
    Api {
        /// File or directory substring to scope the surface to (default: repo).
        path: Option<String>,
        /// Only this kind (e.g. function, struct, class, interface).
        #[arg(long)]
        kind: Option<String>,
        /// Report how the exported surface differs from this Git ref instead:
        /// what was added, removed, or changed shape.
        #[arg(long, value_name = "REF")]
        compare_to: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Definitions that reference a symbol (its callers), test sites flagged.
    Callers {
        /// Exact symbol name.
        name: String,
        /// Restrict to callers located in paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Transitive hops up the call tree (1 = direct callers). Each result is
        /// tagged with its depth.
        #[arg(long, default_value_t = 1, value_parser = hops)]
        depth: usize,
        /// Import-scoped resolution (direct callers only): drop callers whose file
        /// can't reach a definition of the name through imports, and tag the rest
        /// `via` same-file / imported / ambiguous. Heuristic; prunes false
        /// positives at the risk of dropping real callers behind unresolved imports.
        #[arg(long)]
        resolved: bool,
        /// Keep only call sites — `NAME(` — excluding a bare identifier, field, or
        /// variable of the same name (direct callers only).
        #[arg(long)]
        calls: bool,
        /// Keep only calls qualified by this receiver/type — `RECV.NAME` /
        /// `RECV::NAME` / `RECV->NAME` — to disambiguate a common method name.
        #[arg(long, value_name = "RECV")]
        on: Option<String>,
        /// Show the actual call expression at each site (one representative example
        /// per caller): the row becomes `path:line:` the source line. For every
        /// call site instead of one per caller, use `references NAME --calls`.
        #[arg(long)]
        examples: bool,
        /// Maximum rows to return (0 = unlimited).
        ///
        /// Every other list command has had this; these two did not, and they are
        /// the ones most likely to return a lot — `callers serverLog` in redis is
        /// 227 rows and 11 KB with no way to bound it.
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Definitions a symbol references (its callees, name-based).
    Callees {
        /// Exact symbol name.
        name: String,
        /// Scope to the definition(s) of the symbol in matching paths.
        #[arg(long)]
        path: Option<String>,
        /// Transitive hops down the call tree (1 = direct callees).
        #[arg(long, default_value_t = 1, value_parser = hops)]
        depth: usize,
        /// Import-scoped resolution (direct callees only): keep only callees whose
        /// definition file `name`'s own file can reach through imports (or the same
        /// file), and tag each `via` same-file / imported. Heuristic; prunes the
        /// name-based over-match (unrelated same-named definitions in unreachable
        /// files) at the risk of dropping a real callee behind an unresolved import.
        #[arg(long)]
        resolved: bool,
        /// Show the call as written, with its receiver — `subprocess.run`, not
        /// `run` — plus the line it sits on, so a name-based over-match is
        /// visible without having to suspect it first.
        #[arg(long)]
        examples: bool,
        /// Maximum rows to return (0 = unlimited).
        ///
        /// Every other list command has had this; these two did not, and they are
        /// the ones most likely to return a lot — `callers serverLog` in redis is
        /// 227 rows and 11 KB with no way to bound it.
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// The shortest call chain from one symbol to another (`from` calls … calls
    /// `to`), if one exists — reachability with the route shown. Name-based.
    /// (Renamed from `path`: in a code CLI "path" reads as a *file* path first,
    /// and `--path` scopes fifteen other commands, so `codeq path a b` scanned as
    /// "the path to file a". The alias is gone — keeping it meant `codeq path
    /// src/db.rs` failed with "required argument <TO>" instead of "no such
    /// command", making the likely mistake harder to diagnose, not easier.)
    #[command(name = "callpath")]
    Callpath {
        /// The symbol the chain starts at.
        from: String,
        /// The symbol the chain must reach.
        to: String,
        /// Maximum chain length to search (hops).
        #[arg(long, default_value_t = 8)]
        max_depth: usize,
        /// Scope the start symbol to definitions in matching paths — disambiguates
        /// a common/cross-language `from` (e.g. `main`).
        #[arg(long)]
        path: Option<String>,
    },
    /// Symbol dossier WITHOUT source: definition(s) + callers + callees, with full
    /// metadata on each (doc comment, signature, owner, which callers are tests).
    /// Use `explore` for the same neighbourhood WITH verbatim source but trimmed
    /// metadata — source is the axis these two differ on.
    About {
        /// Exact symbol names, in argument order.
        #[arg(required = true, num_args = 1..)]
        name: Vec<String>,
        /// Scope the definition and callees to matching paths (disambiguates
        /// a common name); callers stay repo-wide.
        #[arg(long)]
        path: Option<String>,
        /// Drop callers and callees whose file cannot reach a definition of the
        /// name through imports, and tag the rest `via`.
        #[arg(long)]
        resolved: bool,
    },
    /// Symbol dossier WITH source: definition + callers + callees, plus the
    /// verbatim source of that whole neighbourhood, budgeted and grouped by file.
    /// Structure and code in one shot, instead of definitions then callers then
    /// several reads. Start here to understand or change a symbol. Use `about` for
    /// the same neighbourhood WITHOUT source but with fuller per-node metadata.
    Explore {
        /// Exact symbol name to explore. Omit it when using --at.
        #[arg(required_unless_present = "at", conflicts_with = "at")]
        name: Option<String>,
        /// Explore whatever definition encloses this position, given as
        /// `<path>:<line>`.
        ///
        /// For turning a stack frame, a diff hunk or a search hit straight into
        /// its neighbourhood without first resolving the name by hand. The
        /// position also scopes the lookup, so a common name cannot pull in
        /// same-named definitions from other files.
        #[arg(long, value_name = "FILE:LINE")]
        at: Option<String>,
        /// Scope the definition and callees to matching paths (disambiguates a
        /// common name); callers stay repo-wide.
        #[arg(long)]
        path: Option<String>,
        /// Drop callers and callees whose file cannot reach a definition of the
        /// name through imports, and tag the rest `via`.
        ///
        /// Name matching is textual, so in a polyglot repo a Go function's
        /// neighbourhood fills with same-named TypeScript from an unrelated
        /// program — and here that spends the token budget as well as the
        /// reader's attention.
        #[arg(long)]
        resolved: bool,
        /// Include source for ambiguous callee candidates as well as listing them.
        #[arg(long)]
        include_ambiguous: bool,
        /// Approximate token budget for the included source (the definition is
        /// always included; callers and callees fill the rest).
        ///
        /// The default is sized for a first look: the definition and its nearest
        /// neighbours. What did not fit is listed under `omitted`, so raise this
        /// when that list holds something you need.
        #[arg(long, default_value_t = 4000)]
        max_tokens: u64,
        /// Approximate budget for the full explore response, including metadata.
        #[arg(long, default_value_t = 8000, value_parser = clap::value_parser!(u64).range(256..))]
        max_response_tokens: u64,
        /// Resume an incomplete explore response. Reuse only with an unchanged result.
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
    },
    /// Word-boundary references to an exact symbol name, with enclosing symbol.
    References {
        /// Exact symbol name to find references to.
        name: String,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Restrict to a language.
        #[arg(long)]
        language: Option<String>,
        /// Maximum matches to return (0 = unlimited).
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Import-scoped resolution: drop references whose file can't reach a
        /// definition of the name through imports, and tag the rest `via`
        /// same-file / imported / ambiguous. Heuristic (opt-in) — the default is
        /// a safe textual superset.
        #[arg(long)]
        resolved: bool,
        /// Keep only call sites — `NAME(` — excluding bare mentions of the name.
        #[arg(long)]
        calls: bool,
        /// Keep only occurrences qualified by this receiver/type — `RECV.NAME` /
        /// `RECV::NAME` / `RECV->NAME`.
        #[arg(long, value_name = "RECV")]
        on: Option<String>,
    },
    /// Types that implement or extend a trait / interface / base class — the
    /// inheritance edges the call graph doesn't cover (`impl T for X` in Rust,
    /// `extends`/`implements` in Java/TS/PHP, `class X(Base)` in Python, …). Go
    /// (structural typing) and C have none. Name-based.
    Implementors {
        /// The trait / interface / base class name.
        name: String,
        /// Restrict to one relation: `implements`, `extends`, or `references`
        /// (a SQL foreign key, which is an edge but not inheritance).
        #[arg(long)]
        relation: Option<String>,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// The traits / interfaces / base classes a type extends or implements — the
    /// reverse of `implementors`. The names as written at the declaration, not
    /// transitively resolved. Name-based.
    Supertypes {
        /// The deriving type (class / struct / impl target).
        name: String,
        /// Restrict to one relation: `implements`, `extends`, or `references`
        /// (a SQL foreign key, which is an edge but not inheritance).
        #[arg(long)]
        relation: Option<String>,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Definitions with no reference anywhere outside their own definition site
    /// — candidate dead code. Conservative and name-based: a mention in a comment
    /// or string counts as a use, and a same-named symbol elsewhere masks it, so
    /// it under-reports. `visibility` is shown so a public symbol (maybe external
    /// API, not dead) is distinguishable from a private one (safe to remove).
    Unused {
        /// Restrict the reported definitions to paths containing this substring
        /// (the usage check stays repo-wide).
        path: Option<String>,
        /// Only this kind (e.g. function, struct, class).
        #[arg(long)]
        kind: Option<String>,
        /// Also report definitions in test files (excluded by default, since
        /// test entry points are invoked by the runner, not referenced by name).
        #[arg(long)]
        include_tests: bool,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Definitions registered as framework entry points by syntax — for example
    /// `@app.get`, `@click.command`, or a plugin registration decorator. Tests
    /// are discovered entry points too, but are opt-in to keep production routes
    /// and commands easy to inspect.
    Entrypoints {
        /// Restrict to definitions in paths containing this substring.
        path: Option<String>,
        /// Include definitions structurally marked as tests.
        #[arg(long)]
        include_tests: bool,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// The repo's directory SHAPE, folded to a depth, with each directory's file
    /// and symbol counts aggregated from everything below it. Unlike `tree`(1) it
    /// is ignore-aware (no node_modules/.git noise) and says where the *code* is,
    /// not just where files are; unlike `map` it keeps the nesting.
    Tree {
        /// Restrict to paths containing this substring.
        path: Option<String>,
        /// Fold below this nesting level (counts still include everything below).
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Restrict to one language.
        ///
        /// Often what you actually want in an asset-heavy repo: `--min-symbols`
        /// counts markdown headings as definitions (they are), so a tree of
        /// per-country READMEs can outrank the source directory.
        #[arg(long)]
        language: Option<String>,
        /// Hide directories holding fewer than this many definitions — jumps
        /// straight to where the code lives in a data- or asset-heavy repo.
        #[arg(long, default_value_t = 0)]
        min_symbols: i64,
        /// Also list individual files (within --depth).
        #[arg(long)]
        files: bool,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Definitions ranked by how often their name is referenced in code — the
    /// inverse of `unused`, for "what is load-bearing here?" in unfamiliar code.
    /// Counts are per *name* (name-based like every graph command), so a short
    /// common name ranks high because many unrelated symbols spell it the same
    /// way: read a row as "this identifier carries weight", then use
    /// `callers NAME --calls` for the precise question.
    Hotspots {
        /// Restrict the reported definitions to paths containing this substring
        /// (the count stays repo-wide).
        path: Option<String>,
        /// Only this kind (e.g. function, struct, class).
        #[arg(long)]
        kind: Option<String>,
        /// Only this language.
        #[arg(long)]
        language: Option<String>,
        /// Count only call sites — `NAME(` — instead of every mention. Cuts the
        /// noise from names that are also common variables (a function `s` scores
        /// high otherwise, because every `s` in the repo looks like a reference).
        #[arg(long)]
        calls: bool,
        /// Drop definitions referenced fewer than this many times.
        #[arg(long, default_value_t = 1)]
        min_refs: i64,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Exported definitions that no test refers to — `api` composed with `tests`.
    ///
    /// Public only, and heuristic in the same way `tests` is, so it over-reports
    /// coverage rather than under-report it.
    Untested {
        /// Restrict to paths containing this substring.
        path: Option<String>,
        /// Restrict to one kind (function, struct, class, …).
        #[arg(long)]
        kind: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Test functions that reference a symbol — "which tests exercise X".
    /// Heuristic (test-file path or test-like function name), name-based.
    Tests {
        /// Exact symbol name the tests should reference.
        name: String,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum tests to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// The definition(s) enclosing a location, e.g. src/foo.rs:412.
    At {
        /// Location as <path>:<line>.
        location: String,
    },
    /// Read a file, folding long function bodies so you expand only what you
    /// need (text output, not JSON).
    Read {
        /// File(s) to read (repo-relative path or basename). A range can ride on
        /// the path — `a.rs:40-80`, `a.rs:12`, `a.rs:5-9,80-90` — so several
        /// slices of several files are one call.
        #[arg(required_unless_present = "at", conflicts_with = "at")]
        file: Vec<String>,
        /// Read the innermost definition enclosing <path>:<line>, without guessing
        /// a line range. Repeat to read several positions; overlapping source is emitted once.
        #[arg(long, conflicts_with_all = ["file", "lines", "symbol", "full"])]
        at: Vec<String>,
        /// Show only these lines, verbatim (e.g. 40-80). Repeatable, or
        /// comma-separated: 40-80,120-130.
        #[arg(long, value_delimiter = ',')]
        lines: Vec<String>,
        /// Show only the definition(s) with this name, verbatim.
        #[arg(long)]
        symbol: Option<String>,
        /// Return the whole file without folding.
        #[arg(long)]
        full: bool,
        /// Exact bytes with no line-number prefix — ready to paste as an editor's
        /// search text (old_string). With --symbol, requires a single match.
        #[arg(long)]
        raw: bool,
    },
    /// List import statements (raw, unresolved module strings).
    Imports {
        /// Restrict to importing files whose path contains this substring.
        path: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Files that import a module matching the query. The query must be a whole
    /// name in the import (`sr` matches `./locale/sr`, not `../src`), and a file
    /// path also drops imports that resolve to a different file.
    Importers {
        /// Module token or file path to match against import statements.
        /// A path (e.g. `src/db.rs`) is reduced to its stem (`db`).
        query: String,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Internal files a file depends on, resolved from its imports (transitive
    /// with `--depth`). `--cycles` reports every import cycle in the repo.
    /// Resolution is heuristic (import segments matched to file stems).
    Deps {
        /// The file to start from (a path or unique suffix). Omit with `--cycles`.
        file: Option<String>,
        /// Walk transitive dependencies to this many hops (1 = direct only).
        #[arg(long, default_value_t = 1, value_parser = hops)]
        depth: usize,
        /// Report every import cycle in the repo instead of one file's deps.
        #[arg(long)]
        cycles: bool,
        /// Imports that resolve to no file in the repo: third-party dependencies,
        /// and broken local references (a markdown link or ./path to nothing).
        #[arg(long)]
        external: bool,
    },
    /// Structural (AST) search: match a code pattern with `$VAR` metavariables.
    /// Parsed with the language's grammar, so it matches syntax, not text —
    /// e.g. `codeq astgrep '$X.unwrap()' --language rust`.
    Astgrep {
        /// The code pattern, e.g. `$X.unwrap()` or `if $C { $$ }`. `$NAME`/`$_`
        /// stand for any node; a repeated `$NAME` must match identically.
        #[arg(required_unless_present = "explain")]
        pattern: Option<String>,
        /// Language the pattern is written in (required unless --explain, which
        /// takes it from the file).
        #[arg(long, required_unless_present = "explain")]
        language: Option<String>,
        /// Show the parse tree at <file>:<line> instead of matching a pattern.
        ///
        /// A pattern must name the grammar's node kinds exactly, and there was no
        /// way to discover them from inside codeq — a wrong guess returns an empty
        /// result indistinguishable from "no matches". This prints the kinds and
        /// field names actually covering that line, which is what a pattern is
        /// written against.
        #[arg(long, value_name = "FILE:LINE", conflicts_with_all = ["pattern", "language"])]
        explain: Option<String>,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum matches, or matching definitions with --source (0 = unlimited).
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Return each enclosing definition once, ranked by structural match count.
        #[arg(long, conflicts_with = "explain")]
        source: bool,
        /// Source token budget (approximate, 4 bytes per token).
        #[arg(long, default_value_t = 12000, requires = "source")]
        max_tokens: u64,
    },
    /// Content search; each match annotated with its enclosing definition.
    #[command(alias = "grep", alias = "rg")]
    Search {
        /// Pattern to search for (regex by default).
        #[arg(required_unless_present = "regexp")]
        pattern: Option<String>,
        /// Files or directories to search, like grep: `codeq search TODO src`.
        ///
        /// Each is matched the way `--path` is — as a path *substring* — so a
        /// directory scopes everything under it and a bare filename finds that
        /// file wherever it lives. Several are OR'd, which is what a shell glob
        /// (`src/*.rs`) expands to. Composes with `--path` and `-g` by AND.
        ///
        /// A path that selects no indexed file but exists on disk — gitignored
        /// build output, a log, a file outside the repository — is searched
        /// from disk instead, with a note saying so. Those rows carry no
        /// `[symbol]`, and `--in`/`--language` skip them.
        paths: Vec<String>,
        /// A pattern, repeatable: `-e A -e B` matches either, as in grep. With
        /// it every positional is a path.
        #[arg(long, short = 'e', value_name = "PATTERN")]
        regexp: Vec<String>,
        /// Treat the pattern as a literal string, not a regex.
        #[arg(long, short = 'F', alias = "fixed-strings")]
        fixed: bool,
        /// Print only the matched text, one row per occurrence (grep's -o).
        #[arg(long, short = 'o')]
        only_matching: bool,
        /// Print the lines that do NOT match (grep's -v).
        #[arg(long, short = 'v')]
        invert_match: bool,
        /// Accepted for grep/ripgrep compatibility and has no effect: a pattern
        /// already matches across lines (`foo\s+bar`), reported at its first.
        #[arg(long, short = 'U', hide = true)]
        multiline: bool,
        /// Case-insensitive matching.
        #[arg(long, short = 'i')]
        ignore_case: bool,
        /// Match whole words only.
        #[arg(long, short = 'w')]
        word: bool,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Restrict to a language. `-t`/`--type` are accepted for ripgrep
        /// compatibility, with its short names (`py`, `js`, `ts`, `rs`).
        #[arg(long, short = 't', alias = "type")]
        language: Option<String>,
        /// Include/exclude glob (repeatable); prefix ! to exclude, e.g. -g '*.rs' -g '!tests/'.
        #[arg(long, short = 'g')]
        glob: Vec<String>,
        /// Restrict matches to code, comments, or strings (parsed languages only).
        #[arg(long = "in", value_name = "code|comments|strings")]
        in_kind: Option<String>,
        /// Show N lines of context before and after each match.
        #[arg(long, short = 'C', default_value_t = 0)]
        context: usize,
        /// Show N lines before each match.
        #[arg(long, short = 'B', default_value_t = 0)]
        before: usize,
        /// Show N lines after each match.
        #[arg(long, short = 'A', default_value_t = 0)]
        after: usize,
        /// Output only the paths of files containing a match.
        #[arg(long = "files-with-matches", short = 'l')]
        files_with_matches: bool,
        /// Output only the paths of files with no match (grep's -L).
        #[arg(
            long = "files-without-match",
            short = 'L',
            conflicts_with = "files_with_matches"
        )]
        files_without_match: bool,
        /// Output the count of matching lines per file.
        #[arg(long, short = 'c')]
        count: bool,
        /// Rank enclosing definitions by matching-line count (descending), then path and position.
        #[arg(long, conflicts_with_all = ["files_with_matches", "files_without_match", "count", "stdin"])]
        symbols: bool,
        /// Return each matching definition's source once, budgeting in the same rank order as --symbols.
        #[arg(long, conflicts_with_all = ["files_with_matches", "files_without_match", "count", "stdin"])]
        source: bool,
        /// Maximum source tokens with --source (0 = unlimited).
        #[arg(long, default_value_t = 12000, requires = "source")]
        max_tokens: u64,
        /// Maximum results to return (0 = unlimited).
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Search text piped in on stdin instead of the repository index.
        ///
        /// Requires no repo and reads no index. Pass --language to also get
        /// --in code|comments|strings and the enclosing [symbol] on each match;
        /// without it the scan is exact but has neither.
        #[arg(long)]
        stdin: bool,
    },
    /// Assemble token-budgeted context from explicit seeds.
    Context {
        /// Seed symbols (comma-separated), e.g. authenticate,Session.
        #[arg(long, value_delimiter = ',')]
        symbols: Vec<String>,
        /// Seed from changed files (relative to --compare-to).
        #[arg(long)]
        changed: bool,
        /// Ref to compare against for --changed.
        #[arg(long, default_value = "main")]
        compare_to: String,
        /// Restrict seeds to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Approximate token budget for included chunks.
        #[arg(long, default_value_t = 12000)]
        max_tokens: u64,
        /// Also include definitions referenced by name in the seed chunks
        /// (one deterministic hop). Off by default: the default returns exactly
        /// the seeds requested.
        #[arg(long)]
        expand: bool,
        /// With --expand, keep only referenced definitions the seed files can
        /// reach through imports.
        ///
        /// Expansion matches by name, so a seed mentioning a common name (`new`,
        /// `open`, `build`) otherwise pulls in every same-named definition in the
        /// repo and spends the budget on code the seed cannot call. Same
        /// best-effort import heuristic as `--resolved` elsewhere: it narrows a
        /// superset, it does not resolve types.
        #[arg(long, requires = "expand")]
        resolved: bool,
        /// Omit chunk source text (return provenance only).
        #[arg(long)]
        no_text: bool,
        /// Include the explanatory query plan (seeds, modes, ordering).
        #[arg(long)]
        plan: bool,
    },
    /// EVERY TYPE RELATION as an edge list: the whole inheritance or schema graph
    /// rather than one node's neighbours.
    ///
    /// Relations only, and deliberately not calls. A relation edge is read off
    /// the parse tree (`impl T for X`, a SQL `REFERENCES`), so the graph is
    /// exact; the call graph is name-based and every hop compounds the
    /// over-match, so a whole-repo one would be mostly plausible-looking noise.
    /// Use `callers`/`callees --depth N` there — rooted at a symbol, small
    /// enough to check.
    Graph {
        /// Only this relation: `implements`, `extends`, or `references` (SQL FK).
        #[arg(long)]
        relation: Option<String>,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Restrict to a language.
        #[arg(long)]
        language: Option<String>,
        /// Emit a Mermaid `graph LR` document instead of edge rows.
        ///
        /// A document, not rows: this leaves the `path:line:` contract on
        /// purpose, as `read --raw` does, because the point is to paste it
        /// somewhere it renders.
        #[arg(long)]
        mermaid: bool,
        /// Maximum edges to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// REPO BROKEN DOWN for orientation: counts grouped by language, directory,
    /// imported module and file — what is here and where the weight sits. Start
    /// here in an unfamiliar codebase; `--totals` for the one-line summary and
    /// `tree` when the nesting itself is what you need.
    ///
    /// Each list is ordered by its visible count (files/importers/definitions)
    /// descending, ties broken by name/path.
    Map {
        /// Max rows per list (0 = unlimited).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Whole-repo totals and the per-language breakdown only.
        ///
        /// Skips the four sections that scan rows rather than aggregate them, so
        /// it stays flat as the repo grows: ~30 ms where full `map` is 200 ms on
        /// a 12k-file tree. This replaces the old `stats` command, which returned
        /// a strict subset of `map` and cost a second name to choose between.
        #[arg(long)]
        totals: bool,
    },
    /// Describe what codeq can do: commands, languages, fact kinds, schema.
    Capabilities {},
    /// Print this executable's bundled agent guide, or check a skill file against it.
    /// Works outside a repository and never changes files.
    Guide {
        /// Check that this file matches the bundled guide instead of printing it.
        #[arg(long)]
        check: Option<PathBuf>,
    },
    /// Run as an MCP server over stdio (for agent hosts to call codeq natively).
    Mcp,
    /// Set codeq up for detected coding agents: register its MCP server and install
    /// the codeq skill for agents that read one. Only touches detected agents
    /// unless `--agent` names one.
    Install {
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Remove codeq from each agent's config and delete the skill, instead of
        /// adding them.
        #[arg(long)]
        uninstall: bool,
        /// Target one agent, creating its configuration if absent.
        #[arg(long, value_parser = install::AGENTS.to_vec())]
        agent: Option<String>,
        /// Register the MCP server only, leaving the skill alone.
        #[arg(long)]
        no_skill: bool,
        /// Back up and replace customized codeq entries or skills. Uninstall restores them.
        #[arg(long, conflicts_with = "uninstall")]
        force: bool,
    },
    /// Remove managed agent registrations and skills, preserving customized content.
    Uninstall {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_parser = install::AGENTS.to_vec())]
        agent: Option<String>,
        /// Leave installed skills alone.
        #[arg(long)]
        no_skill: bool,
    },
    /// Check registrations, bundled guides, and MCP startup without changing configuration.
    Doctor {
        #[arg(long, value_parser = install::AGENTS.to_vec())]
        agent: Option<String>,
    },
}

/// A hop count for a graph walk. `1` is the direct neighbours, so `0` names no
/// walk at all; it used to run as `1` (callers, callees) or return nothing
/// (deps), neither of which is what was asked for.
fn hops(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1 (1 = direct)".to_string()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

/// Restore the default SIGPIPE behaviour (Rust sets it to ignore, which turns a
/// closed pipe — e.g. `codeq read … | head` — into a panic). With the default,
/// codeq terminates silently like `cat`/`grep`.
#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
#[cfg(not(unix))]
fn reset_sigpipe() {}

fn main() -> ExitCode {
    reset_sigpipe();
    let cli = Cli::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // `--root` pins the tree; `PINNED_ROOT` carries that to `Repo::at` without
    // threading a parameter through every command signature.
    let cwd = match &cli.root {
        Some(r) => {
            commands::pin_root();
            if r.is_absolute() {
                r.clone()
            } else {
                cwd.join(r)
            }
        }
        None => cwd,
    };

    let out = cli.output_options();

    match dispatch(&cwd, cli.command, out) {
        Ok(()) => ExitCode::SUCCESS,
        // A raw "database is locked" reads like corruption. It is another
        // codeq still writing, which is routine during a long cold build.
        Err(e) if db::is_busy(&e) => {
            eprintln!(
                "codeq: another codeq process is still writing this repository's index and did \
                 not finish within {:?}. The index is not damaged: retry when that run \
                 completes, or set CODEQ_BUSY_TIMEOUT_MS to wait longer.",
                db::busy_timeout()
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("codeq: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Run one subcommand: unpack its clap arguments and hand them to the
/// matching `commands::cmd_*`.
pub(crate) fn dispatch(cwd: &Path, command: Command, out: commands::Out) -> anyhow::Result<()> {
    match command {
        Command::Index { rebuild, compact } => commands::cmd_index(cwd, rebuild, compact, out),
        Command::Files {
            pattern,
            language,
            limit,
        } => commands::cmd_files(cwd, pattern.as_deref(), language.as_deref(), limit, out),
        Command::Changed {
            compare_to,
            symbols,
            limit,
        } => commands::cmd_changed(cwd, &compare_to, symbols, limit, out),
        Command::Clones {
            path,
            kind,
            min_nodes,
            include_tests,
            limit,
        } => commands::cmd_clones(
            cwd,
            path.as_deref(),
            kind.as_deref(),
            min_nodes,
            include_tests,
            limit,
            out,
        ),
        Command::Affected {
            compare_to,
            depth,
            limit,
        } => commands::cmd_affected(cwd, &compare_to, depth, limit, out),
        Command::Symbols {
            pattern,
            kind,
            owner,
            language,
            path,
            signatures,
            limit,
        } => commands::cmd_symbols(
            cwd,
            commands::SymbolsArgs {
                pattern: pattern.as_deref(),
                kind: kind.as_deref(),
                owner: owner.as_deref(),
                language: language.as_deref(),
                path: path.as_deref(),
                signatures,
                limit,
            },
            out,
        ),
        Command::Outline { file, docs } => commands::cmd_outline(cwd, &file, docs, out),
        Command::Definitions {
            symbol,
            path,
            limit,
        } => commands::cmd_definitions(cwd, &symbol, path.as_deref(), limit, out),
        Command::Api {
            path,
            kind,
            compare_to,
            limit,
        } => commands::cmd_api(
            cwd,
            path.as_deref(),
            kind.as_deref(),
            compare_to.as_deref(),
            limit,
            out,
        ),
        Command::Callers {
            name,
            path,
            depth,
            resolved,
            calls,
            on,
            examples,
            limit,
        } => commands::cmd_callers(
            cwd,
            commands::CallersArgs {
                name: &name,
                path: path.as_deref(),
                depth,
                resolved,
                call_only: calls,
                qualifier: on.as_deref(),
                examples,
                limit,
            },
            out,
        ),
        Command::Callees {
            name,
            path,
            depth,
            resolved,
            examples,
            limit,
        } => commands::cmd_callees(
            cwd,
            commands::CalleesArgs {
                name: &name,
                path: path.as_deref(),
                depth,
                resolved,
                examples,
                limit,
            },
            out,
        ),
        Command::Callpath {
            from,
            to,
            max_depth,
            path,
        } => commands::cmd_callpath(cwd, &from, &to, max_depth, path.as_deref(), out),
        Command::About {
            name,
            path,
            resolved,
        } => commands::cmd_about(cwd, &name, path.as_deref(), resolved, out),
        Command::Explore {
            name,
            at,
            path,
            resolved,
            include_ambiguous,
            max_tokens,
            max_response_tokens,
            cursor,
        } => commands::cmd_explore(
            cwd,
            commands::ExploreArgs {
                name: name.as_deref(),
                at: at.as_deref(),
                path: path.as_deref(),
                resolved,
                include_ambiguous,
                max_tokens,
                max_response_tokens,
                cursor: cursor.as_deref(),
            },
            out,
        ),
        Command::References {
            name,
            path,
            language,
            limit,
            resolved,
            calls,
            on,
        } => commands::cmd_references(
            cwd,
            commands::ReferencesArgs {
                name: &name,
                path: path.as_deref(),
                language: language.as_deref(),
                limit,
                resolved,
                call_only: calls,
                qualifier: on.as_deref(),
            },
            out,
        ),
        Command::Implementors {
            name,
            relation,
            path,
            limit,
        } => {
            commands::cmd_implementors(cwd, &name, relation.as_deref(), path.as_deref(), limit, out)
        }
        Command::Supertypes {
            name,
            relation,
            path,
            limit,
        } => commands::cmd_supertypes(cwd, &name, relation.as_deref(), path.as_deref(), limit, out),
        Command::Unused {
            path,
            kind,
            include_tests,
            limit,
        } => commands::cmd_unused(
            cwd,
            path.as_deref(),
            kind.as_deref(),
            include_tests,
            limit,
            out,
        ),
        Command::Entrypoints {
            path,
            include_tests,
            limit,
        } => commands::cmd_entrypoints(cwd, path.as_deref(), include_tests, limit, out),
        Command::Tree {
            path,
            language,
            depth,
            min_symbols,
            files,
            limit,
        } => commands::cmd_tree(
            cwd,
            commands::TreeArgs {
                path: path.as_deref(),
                language: language.as_deref(),
                depth,
                min_symbols,
                files_too: files,
                limit,
            },
            out,
        ),
        Command::Hotspots {
            path,
            kind,
            language,
            calls,
            min_refs,
            limit,
        } => commands::cmd_hotspots(
            cwd,
            commands::HotspotsArgs {
                path: path.as_deref(),
                kind: kind.as_deref(),
                language: language.as_deref(),
                calls_only: calls,
                min_refs,
                limit,
            },
            out,
        ),
        Command::Tests { name, path, limit } => {
            commands::cmd_tests(cwd, &name, path.as_deref(), limit, out)
        }
        Command::At { location } => commands::cmd_at(cwd, &location, out),
        Command::Read { at, raw, .. } if !at.is_empty() => {
            commands::cmd_read_at(cwd, &at, raw, out)
        }
        Command::Read {
            file,
            lines,
            symbol,
            full,
            raw,
            ..
        } => commands::cmd_read(cwd, &file, &lines, symbol.as_deref(), full, raw, out),
        Command::Imports { path, limit } => commands::cmd_imports(cwd, path.as_deref(), limit, out),
        Command::Importers { query, limit } => commands::cmd_importers(cwd, &query, limit, out),
        Command::Deps {
            file,
            depth,
            cycles,
            external,
        } => commands::cmd_deps(cwd, file.as_deref(), depth, cycles, external, out),
        Command::Astgrep {
            pattern,
            language,
            path,
            limit,
            explain,
            source,
            max_tokens,
        } => match explain {
            Some(loc) => commands::cmd_astgrep_explain(cwd, &loc, out),
            // clap's `required_unless_present` guarantees both are set here.
            None => commands::cmd_astgrep(
                cwd,
                &pattern.unwrap_or_default(),
                &language.unwrap_or_default(),
                path,
                limit,
                source.then_some(max_tokens),
                out,
            ),
        },
        Command::Search {
            pattern,
            mut paths,
            regexp,
            fixed,
            only_matching,
            invert_match,
            multiline: _,
            files_without_match,
            ignore_case,
            word,
            path,
            language,
            glob,
            in_kind,
            context,
            before,
            after,
            files_with_matches,
            count,
            symbols,
            source,
            max_tokens,
            limit,
            stdin,
        } => {
            // With `-e`, the first positional is a path like the rest.
            let patterns = if regexp.is_empty() {
                pattern.into_iter().collect()
            } else {
                paths.splice(0..0, pattern);
                regexp
            };
            commands::cmd_search(
                cwd,
                commands::SearchArgs {
                    patterns: &patterns,
                    paths: &paths,
                    files_without_match,
                    only_matching,
                    invert: invert_match,
                    fixed,
                    ignore_case,
                    word,
                    path: path.as_deref(),
                    language: language.as_deref(),
                    globs: &glob,
                    in_kind: in_kind.as_deref(),
                    before: before.max(context),
                    after: after.max(context),
                    files_with_matches,
                    symbols,
                    source,
                    max_tokens,
                    count,
                    limit,
                    stdin,
                },
                out,
            )
        }
        Command::Context {
            symbols,
            changed,
            compare_to,
            path,
            max_tokens,
            expand,
            resolved,
            no_text,
            plan,
        } => commands::cmd_context(
            cwd,
            context::Request {
                symbols,
                changed,
                compare_to,
                path,
                max_tokens,
                expand,
                resolved,
                include_text: !no_text,
                plan,
            },
            out,
        ),
        Command::Members {
            name,
            path,
            kind,
            limit,
        } => commands::cmd_members(cwd, &name, path.as_deref(), kind.as_deref(), limit, out),
        Command::Untested { path, kind, limit } => {
            commands::cmd_untested(cwd, path.as_deref(), kind.as_deref(), limit, out)
        }
        Command::Graph {
            relation,
            path,
            language,
            mermaid,
            limit,
        } => commands::cmd_graph(
            cwd,
            relation.as_deref(),
            path.as_deref(),
            language.as_deref(),
            mermaid,
            limit,
            out,
        ),
        Command::Map { limit, totals } => commands::cmd_map(cwd, limit, totals, out),
        Command::Capabilities {} => {
            // Ask clap what subcommands exist instead of keeping a second list in
            // sync by hand — the hand-written one had silently gone stale.
            let cmd = <Cli as clap::CommandFactory>::command();
            let mut names: Vec<&str> = cmd
                .get_subcommands()
                .map(|c| c.get_name())
                .filter(|n| *n != "help")
                .collect();
            names.sort_unstable();
            commands::cmd_capabilities(&names, out)
        }
        Command::Guide { check } => commands::cmd_guide(check.as_deref(), out),
        Command::Mcp => mcp::serve(cwd),
        Command::Install {
            dry_run,
            uninstall,
            agent,
            no_skill,
            force,
        } => commands::cmd_install(dry_run, uninstall, agent.as_deref(), no_skill, force, out),
        Command::Uninstall {
            dry_run,
            agent,
            no_skill,
        } => commands::cmd_install(dry_run, true, agent.as_deref(), no_skill, false, out),
        Command::Doctor { agent } => commands::cmd_doctor(agent.as_deref(), out),
    }
}
