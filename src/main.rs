//! codeq — a deterministic single source of truth for repository intelligence,
//! built for AI agents.
//!
//! Indexes a repository into SQLite (daemonless, incremental, ignore-aware) and
//! answers structured queries over files, symbols, imports, and file/import
//! aggregates, plus token-budgeted context assembly — all as compact JSON.
//! Results are deterministic and reproducible; codeq reports facts, it does not
//! guess intent or rank by opinion.

mod commands;
mod context;
mod db;
mod envelope;
mod facts;
mod index;
mod repo;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "codeq", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

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
    /// Build or refresh the index for the current repository.
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
        /// Maximum rows to return (0 = unlimited).
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
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
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
        /// Module or path substring to match against import statements.
        query: String,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
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
        /// Restrict to paths containing this substring.
        #[arg(long)]
        path: Option<String>,
        /// Restrict to a language.
        #[arg(long)]
        language: Option<String>,
        /// Maximum matches to return (0 = unlimited).
        #[arg(long, default_value_t = 100)]
        limit: usize,
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
    },
    /// Summary counts for the current index.
    Stats {
    },
    /// Factual aggregation of the index for orientation (grouped counts).
    Map {
        /// Max rows per list (0 = unlimited).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Describe what codeq can do: commands, languages, fact kinds, schema.
    Capabilities {
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let out = commands::Out {
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
            limit,
        } => commands::cmd_changed(&cwd, &compare_to, limit, out),
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
            limit,
        } => commands::cmd_definitions(&cwd, &symbol, limit, out),
        Command::Imports {
            path,
            limit,
        } => commands::cmd_imports(&cwd, path.as_deref(), limit, out),
        Command::Importers {
            query,
            limit,
        } => commands::cmd_importers(&cwd, &query, limit, out),
        Command::Search {
            pattern,
            fixed,
            ignore_case,
            path,
            language,
            limit,
        } => commands::cmd_search(
            &cwd,
            &pattern,
            fixed,
            ignore_case,
            path.as_deref(),
            language.as_deref(),
            limit,
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
            },
            out,
        ),
        Command::Stats {} => commands::cmd_stats(&cwd, out),
        Command::Map { limit } => commands::cmd_map(&cwd, limit, out),
        Command::Capabilities {} => commands::cmd_capabilities(out),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("codeq: {e:#}");
            ExitCode::FAILURE
        }
    }
}
