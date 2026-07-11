//! codeq — a deterministic single source of truth for repository intelligence,
//! built for AI agents.
//!
//! This is the v1 proving spine: index a repo into SQLite, then answer
//! `files`, `changed`, and `stats` with a stable JSON envelope and daemonless
//! freshness. Symbols, imports, and context come next.

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
        /// Emit JSON (currently the only format).
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Files changed relative to a Git ref (merge-base with HEAD).
    Changed {
        /// Ref to compare against, e.g. main.
        #[arg(long, default_value = "main")]
        compare_to: String,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        json: bool,
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
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// The structure of a file top to bottom: definitions, signatures, nesting.
    Outline {
        /// File to outline (repo-relative path or basename).
        file: String,
        /// Include docstrings for each definition.
        #[arg(long)]
        docs: bool,
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Locate the definition(s) of an exact symbol name.
    Definitions {
        /// Exact symbol name to resolve.
        symbol: String,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// List import statements (raw, unresolved module strings).
    Imports {
        /// Restrict to importing files whose path contains this substring.
        path: Option<String>,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Files that import a module matching the query (raw string match).
    Importers {
        /// Module or path substring to match against import statements.
        query: String,
        /// Maximum rows to return (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        json: bool,
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
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Summary counts for the current index.
    Stats {
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Factual aggregation of the index for orientation (grouped counts).
    Map {
        /// Max rows per list (0 = unlimited).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Describe what codeq can do: commands, languages, fact kinds, schema.
    Capabilities {
        #[arg(long, default_value_t = true)]
        json: bool,
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
            json: _,
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
            json: _,
        } => commands::cmd_changed(&cwd, &compare_to, limit, out),
        Command::Symbols {
            pattern,
            kind,
            language,
            path,
            signatures,
            limit,
            json: _,
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
            json: _,
        } => commands::cmd_outline(&cwd, &file, docs, out),
        Command::Definitions {
            symbol,
            limit,
            json: _,
        } => commands::cmd_definitions(&cwd, &symbol, limit, out),
        Command::Imports {
            path,
            limit,
            json: _,
        } => commands::cmd_imports(&cwd, path.as_deref(), limit, out),
        Command::Importers {
            query,
            limit,
            json: _,
        } => commands::cmd_importers(&cwd, &query, limit, out),
        Command::Context {
            symbols,
            changed,
            compare_to,
            path,
            max_tokens,
            expand,
            no_text,
            json: _,
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
        Command::Stats { json: _ } => commands::cmd_stats(&cwd, out),
        Command::Map { limit, json: _ } => commands::cmd_map(&cwd, limit, out),
        Command::Capabilities { json: _ } => commands::cmd_capabilities(out),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("codeq: {e:#}");
            ExitCode::FAILURE
        }
    }
}
