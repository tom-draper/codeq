//! codeq — a deterministic single source of truth for repository intelligence,
//! built for AI agents.
//!
//! Indexes a repository into SQLite (daemonless, incremental, ignore-aware) and
//! answers structured queries over files, symbols, imports, and file/import
//! aggregates, plus token-budgeted context assembly — all as compact JSON.
//! Results are deterministic and reproducible; codeq reports facts, it does not
//! guess intent or rank by opinion.

mod astgrep;
mod commands;
mod context;
mod db;
mod deps;
mod envelope;
mod facts;
mod index;
mod install;
mod mcp;
mod repo;
mod search;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "codeq", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Emit JSON (the `codeq.result.v1` envelope). Default is compact grep-style
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
}

#[derive(Subcommand)]
enum Command {
    /// Force a full rebuild or pre-warm the index. Rarely needed: every other
    /// command auto-refreshes changed files before answering, so you never have
    /// to run this for a query to see the current tree.
    Index {
        /// Discard the existing index and reparse every file.
        #[arg(long)]
        rebuild: bool,
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
    /// For a diff: the definitions it touched, their transitive callers (blast
    /// radius), and the tests exercising any of them — what to re-check and re-run
    /// for the change. Name-based.
    Affected {
        /// Ref to compare against, e.g. main.
        #[arg(long, default_value = "main")]
        compare_to: String,
        /// How many hops up the call tree to include as blast radius.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Maximum test rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// List defined symbols (functions, structs, classes, …).
    Symbols {
        /// Substring to match against symbol names.
        pattern: Option<String>,
        /// Restrict to a symbol kind (e.g. function, struct, class).
        #[arg(long)]
        kind: Option<String>,
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
    /// Locate the definition(s) of an exact symbol name.
    Definitions {
        /// Exact symbol name to resolve.
        symbol: String,
        /// Restrict to definitions in paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum rows to return (0 = unlimited).
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
        #[arg(long, default_value_t = 1)]
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
        /// per caller): the row becomes `path:line:col:` the source line. For every
        /// call site instead of one per caller, use `references NAME --calls`.
        #[arg(long)]
        examples: bool,
    },
    /// Definitions a symbol references (its callees, name-based).
    Callees {
        /// Exact symbol name.
        name: String,
        /// Scope to the definition(s) of the symbol in matching paths.
        #[arg(long)]
        path: Option<String>,
        /// Transitive hops down the call tree (1 = direct callees).
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Import-scoped resolution (direct callees only): keep only callees whose
        /// definition file `name`'s own file can reach through imports (or the same
        /// file), and tag each `via` same-file / imported. Heuristic; prunes the
        /// name-based over-match (unrelated same-named definitions in unreachable
        /// files) at the risk of dropping a real callee behind an unresolved import.
        #[arg(long)]
        resolved: bool,
    },
    /// The shortest call chain from one symbol to another (`from` calls … calls
    /// `to`), if one exists — reachability with the route shown. Name-based.
    Path {
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
    /// Dossier for a symbol: definition(s) + callers + callees, in one call.
    About {
        /// Exact symbol name.
        name: String,
        /// Scope the definition and callees to matching paths (disambiguates
        /// a common name); callers stay repo-wide.
        #[arg(long)]
        path: Option<String>,
    },
    /// One-call answer for a symbol: its definition, callers, and callees, plus
    /// the verbatim source of that whole neighbourhood, budgeted and grouped by
    /// file. Structure and code in one shot, instead of definitions then callers
    /// then several reads.
    Explore {
        /// Exact symbol name to explore.
        name: String,
        /// Scope the definition and callees to matching paths (disambiguates a
        /// common name); callers stay repo-wide.
        #[arg(long)]
        path: Option<String>,
        /// Approximate token budget for the included source (the definition is
        /// always included; callers and callees fill the rest).
        #[arg(long, default_value_t = 16000)]
        max_tokens: u64,
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
        /// Restrict to one relation: `implements` or `extends`.
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
        /// Restrict to one relation: `implements` or `extends`.
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
        /// File to read (repo-relative path or basename).
        file: String,
        /// Show only these lines, verbatim (e.g. 40-80).
        #[arg(long)]
        lines: Option<String>,
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
    /// Files that import a module matching the query (raw string match).
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
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Report every import cycle in the repo instead of one file's deps.
        #[arg(long)]
        cycles: bool,
    },
    /// Structural (AST) search: match a code pattern with `$VAR` metavariables.
    /// Parsed with the language's grammar, so it matches syntax, not text —
    /// e.g. `codeq astgrep '$X.unwrap()' --language rust`.
    Astgrep {
        /// The code pattern, e.g. `$X.unwrap()` or `if $C { $$ }`. `$NAME`/`$_`
        /// stand for any node; a repeated `$NAME` must match identically.
        pattern: String,
        /// Language the pattern is written in (required).
        #[arg(long)]
        language: String,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Maximum matches to return (0 = unlimited).
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Content search; each match annotated with its enclosing definition.
    Search {
        /// Pattern to search for (regex by default).
        pattern: String,
        /// Treat the pattern as a literal string, not a regex.
        #[arg(long)]
        fixed: bool,
        /// Case-insensitive matching.
        #[arg(long, short = 'i')]
        ignore_case: bool,
        /// Match whole words only.
        #[arg(long, short = 'w')]
        word: bool,
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Restrict to a language.
        #[arg(long)]
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
        /// Output the count of matching lines per file.
        #[arg(long, short = 'c')]
        count: bool,
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
        /// Omit chunk source text (return provenance only).
        #[arg(long)]
        no_text: bool,
        /// Include the explanatory query plan (seeds, modes, ordering).
        #[arg(long)]
        plan: bool,
    },
    /// Summary counts for the current index.
    Stats {
    },
    /// Factual aggregation of the index for orientation (grouped counts).
    ///
    /// Each list is ordered by its visible count (files/importers/definitions)
    /// descending, ties broken by name/path.
    Map {
        /// Max rows per list (0 = unlimited).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Describe what codeq can do: commands, languages, fact kinds, schema.
    Capabilities {
    },
    /// Run as an MCP server over stdio (for agent hosts to call codeq natively).
    Mcp,
    /// Register codeq's MCP server with detected AI coding agents (Claude Code,
    /// Cursor, Windsurf, Gemini), writing to each agent's own config. Only touches
    /// detected agents unless `--agent` names one.
    Install {
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Remove codeq from each agent's config instead of adding it.
        #[arg(long)]
        uninstall: bool,
        /// Target just this agent (and create its config if absent): claude-code,
        /// cursor, windsurf, or gemini.
        #[arg(long)]
        agent: Option<String>,
    },
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

    let out = commands::Out {
        json: cli.json,
        meta: cli.meta,
        pretty: cli.pretty,
        fields: cli.fields,
    };

    let result = match cli.command {
        Command::Index { rebuild } => commands::cmd_index(&cwd, rebuild, out),
        Command::Files {
            pattern,
            language,
            limit,
        } => commands::cmd_files(
            &cwd,
            pattern.as_deref(),
            language.as_deref(),
            limit,
            out,
        ),
        Command::Changed {
            compare_to,
            symbols,
            limit,
        } => commands::cmd_changed(&cwd, &compare_to, symbols, limit, out),
        Command::Affected {
            compare_to,
            depth,
            limit,
        } => commands::cmd_affected(&cwd, &compare_to, depth, limit, out),
        Command::Symbols {
            pattern,
            kind,
            language,
            path,
            signatures,
            limit,
        } => commands::cmd_symbols(
            &cwd,
            pattern.as_deref(),
            kind.as_deref(),
            language.as_deref(),
            path.as_deref(),
            signatures,
            limit,
            out,
        ),
        Command::Outline {
            file,
            docs,
        } => commands::cmd_outline(&cwd, &file, docs, out),
        Command::Definitions {
            symbol,
            path,
            limit,
        } => commands::cmd_definitions(&cwd, &symbol, path.as_deref(), limit, out),
        Command::Api { path, kind, limit } => {
            commands::cmd_api(&cwd, path.as_deref(), kind.as_deref(), limit, out)
        }
        Command::Callers {
            name,
            path,
            depth,
            resolved,
            calls,
            on,
            examples,
        } => commands::cmd_callers(
            &cwd,
            &name,
            path.as_deref(),
            depth,
            resolved,
            calls,
            on.as_deref(),
            examples,
            out,
        ),
        Command::Callees { name, path, depth, resolved } => {
            commands::cmd_callees(&cwd, &name, path.as_deref(), depth, resolved, out)
        }
        Command::Path {
            from,
            to,
            max_depth,
            path,
        } => commands::cmd_path(&cwd, &from, &to, max_depth, path.as_deref(), out),
        Command::About { name, path } => {
            commands::cmd_about(&cwd, &name, path.as_deref(), out)
        }
        Command::Explore { name, path, max_tokens } => {
            commands::cmd_explore(&cwd, &name, path.as_deref(), max_tokens, out)
        }
        Command::References {
            name,
            path,
            language,
            limit,
            resolved,
            calls,
            on,
        } => commands::cmd_references(
            &cwd,
            &name,
            path.as_deref(),
            language.as_deref(),
            limit,
            resolved,
            calls,
            on.as_deref(),
            out,
        ),
        Command::Implementors {
            name,
            relation,
            path,
            limit,
        } => commands::cmd_implementors(
            &cwd,
            &name,
            relation.as_deref(),
            path.as_deref(),
            limit,
            out,
        ),
        Command::Supertypes {
            name,
            relation,
            path,
            limit,
        } => commands::cmd_supertypes(
            &cwd,
            &name,
            relation.as_deref(),
            path.as_deref(),
            limit,
            out,
        ),
        Command::Unused {
            path,
            kind,
            include_tests,
            limit,
        } => commands::cmd_unused(
            &cwd,
            path.as_deref(),
            kind.as_deref(),
            include_tests,
            limit,
            out,
        ),
        Command::Tests {
            name,
            path,
            limit,
        } => commands::cmd_tests(&cwd, &name, path.as_deref(), limit, out),
        Command::At { location } => commands::cmd_at(&cwd, &location, out),
        Command::Read {
            file,
            lines,
            symbol,
            full,
            raw,
        } => commands::cmd_read(&cwd, &file, lines.as_deref(), symbol.as_deref(), full, raw, out),
        Command::Imports {
            path,
            limit,
        } => commands::cmd_imports(&cwd, path.as_deref(), limit, out),
        Command::Importers {
            query,
            limit,
        } => commands::cmd_importers(&cwd, &query, limit, out),
        Command::Deps {
            file,
            depth,
            cycles,
        } => commands::cmd_deps(&cwd, file.as_deref(), depth, cycles, out),
        Command::Astgrep {
            pattern,
            language,
            path,
            limit,
        } => commands::cmd_astgrep(&cwd, &pattern, &language, path, limit, out),
        Command::Search {
            pattern,
            fixed,
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
            limit,
            stdin,
        } => commands::cmd_search(
            &cwd,
            commands::SearchArgs {
                pattern: &pattern,
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
                count,
                limit,
                stdin,
            },
            out,
        ),
        Command::Context {
            symbols,
            changed,
            compare_to,
            path,
            max_tokens,
            expand,
            no_text,
            plan,
        } => commands::cmd_context(
            &cwd,
            context::Request {
                symbols,
                changed,
                compare_to,
                path,
                max_tokens,
                expand,
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
        } => commands::cmd_members(&cwd, &name, path.as_deref(), kind.as_deref(), limit, out),
        Command::Stats {} => commands::cmd_stats(&cwd, out),
        Command::Map { limit } => commands::cmd_map(&cwd, limit, out),
        Command::Capabilities {} => commands::cmd_capabilities(out),
        Command::Mcp => mcp::serve(),
        Command::Install {
            dry_run,
            uninstall,
            agent,
        } => commands::cmd_install(dry_run, uninstall, agent.as_deref(), out),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("codeq: {e:#}");
            ExitCode::FAILURE
        }
    }
}
