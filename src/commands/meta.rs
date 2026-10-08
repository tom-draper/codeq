//! Commands about codeq itself: `install`, `capabilities`.

use super::*;

/// `install` — register codeq's MCP server with detected AI coding agents. Writes
/// to each agent's own config (in the home directory), never to a repository, so
/// it needs no index/Session. Honours `--json`/`--pretty` for its own summary.
pub fn cmd_install(
    dry_run: bool,
    uninstall: bool,
    agent: Option<&str>,
    no_skill: bool,
    out: Out,
) -> Result<()> {
    let home = crate::install::home_dir()?;
    let bin = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "codeq".to_string());
    let opts = crate::install::Options {
        dry_run,
        uninstall,
        only: agent.map(String::from),
        bin,
        no_skill,
    };
    let actions = crate::install::apply(&home, &opts)?;

    if out.json {
        let data = serde_json::json!({
            "schema": crate::envelope::RESULT_SCHEMA,
            "data": actions,
        });
        let s = if out.pretty {
            serde_json::to_string_pretty(&data)?
        } else {
            serde_json::to_string(&data)?
        };
        println!("{s}");
    } else {
        for a in &actions {
            let note = a
                .note
                .as_deref()
                .map(|n| format!("  ({n})"))
                .unwrap_or_default();
            println!(
                "{:<12}  {:<6}  {:<15}  {}{}",
                a.agent, a.what, a.action, a.config, note
            );
        }
        let touched = actions
            .iter()
            .filter(|a| !matches!(a.action, "skipped" | "already-current" | "already-absent"))
            .count();
        if touched == 0 && !uninstall {
            println!(
                "\nNo agents updated. Detected agents are configured automatically; \
                 target one explicitly with `--agent <name>` (claude-code, cursor, \
                 windsurf, gemini)."
            );
        } else if !dry_run && !uninstall && touched > 0 {
            println!("\nRestart the agent(s) above to pick up the codeq MCP server and skill.");
        }
    }
    Ok(())
}

/// Self-description so an agent can discover what codeq supports without
/// guessing. Deliberately does not require (or refresh) an index.
///
/// `commands` is passed in from the clap parser rather than written out here.
/// The hand-maintained list drifted: it had gone nine commands stale — including
/// `explore`, the one an agent should reach for first — so the command whose only
/// job is to say what codeq can do was under-selling it by a quarter.
pub fn cmd_capabilities(commands: &[&str], out: Out) -> Result<()> {
    // Distinct languages a file can be classified as, derived from the extension
    // table rather than restated (`json!` cannot host the turbofish inline).
    let mut classified: Vec<&str> = crate::index::EXTENSIONS.iter().map(|(_, l)| *l).collect();
    classified.sort_unstable();
    classified.dedup();
    let data = serde_json::json!({
        "result_schema": RESULT_SCHEMA,
        "index_schema": SCHEMA_VERSION,
        "commands": commands,
        // Per-language support, so "no results" can be told from "not supported".
        "language_support": crate::facts::LANGUAGES
            .iter()
            .filter_map(|l| crate::facts::support(l))
            .collect::<Vec<_>>(),
        // Languages with symbol + import extraction. Derived, not listed: this
        // was a third hand-kept copy, and it had already drifted out of step
        // with `language_support` above (it omitted markdown).
        "extraction_languages": crate::facts::LANGUAGES,
        // Languages recognised for file classification (extraction may be absent).
        "classified_languages": classified,
        // Classified but deliberately not parsed: indexed as files and fully
        // searchable, but they contribute no definitions. Stated so that an
        // empty `symbols` on a .json file reads as "by design", not "broken".
        "data_only_languages": crate::index::NO_EXTRACTION,
        "fact_kinds": [
            "function", "method", "struct", "enum", "union", "trait", "module",
            "constant", "typealias", "macro", "class", "interface", "type", "import",
            "component", "field", "variant", "heading",
        ],
        "notes": {
            "imports": "raw syntactic module strings; not resolved",
            "deps": "an import naming a directory resolves through that directory's conventional entry file (index.js, __init__.py, mod.rs) or any .tf",
            "clones": "structural equality of the syntax tree with identifiers and literals normalised away; exact, never a similarity score. min_nodes filters by how much code is involved",
            "unused": "skips GraphQL/protobuf definitions and Terraform outputs — they exist to be consumed outside the repo, so absence of local references says nothing",
            "members": "field/variant facts carry `owner` (the declaring type); extracted for Rust, Go, Java, TypeScript/TSX, C#, Python, C, C++, PHP, Kotlin, Swift, Protobuf, GraphQL, HCL, Zig, Solidity, Dart, Scala. Python attributes are assignments, so a member is its earliest `self.x =` or class-body assignment",
            "hcl": "block kinds are the block keyword verbatim (resource/variable/module/…), so the vocabulary is open-ended; a block is named by its last label and owned by its type label. A module `source` is an import, but `deps` resolves file stems and a module target is a directory, so it stays unresolved",
            "context_tokenizer": "approx:4-bytes-per-token",
            "freshness": "every command refreshes changed files before answering",
        },
    });
    emit_sessionless(&data, out)
}
