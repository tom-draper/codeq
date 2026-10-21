//! Commands about codeq itself: `install`, `capabilities`.

use super::*;
use anyhow::Context;

pub fn cmd_guide(check: Option<&Path>, out: Out) -> Result<()> {
    let guide = crate::install::SKILL;
    let matches = check
        .map(|path| {
            std::fs::read(path)
                .with_context(|| format!("cannot read guide {}", path.display()))
                .map(|bytes| bytes == guide.as_bytes())
        })
        .transpose()?;
    if out.json {
        let data = match check {
            Some(path) => serde_json::json!({"path": path, "matches": matches}),
            None => serde_json::json!({"guide": guide}),
        };
        let result = serde_json::json!({"schema": crate::envelope::RESULT_SCHEMA, "data": data});
        crate::output::stdout(format_args!(
            "{}\n",
            if out.pretty {
                serde_json::to_string_pretty(&result)?
            } else {
                serde_json::to_string(&result)?
            }
        ));
    } else if let Some(path) = check {
        if matches == Some(true) {
            crate::output::stdout(format_args!(
                "{}: matches this executable's bundled guide\n",
                path.display()
            ));
        }
    } else {
        crate::output::stdout(format_args!("{guide}"));
    }
    if matches == Some(false) {
        anyhow::bail!("guide differs from this executable's bundled guide; run `codeq guide` for current instructions or `codeq install` to update managed skills");
    }
    Ok(())
}

/// `install` — register codeq's MCP server with detected AI coding agents. Writes
/// to each agent's own config (in the home directory), never to a repository, so
/// it needs no index/Session. Honours `--json`/`--pretty` for its own summary.
pub fn cmd_install(
    dry_run: bool,
    uninstall: bool,
    agent: Option<&str>,
    no_skill: bool,
    force: bool,
    out: Out,
) -> Result<()> {
    let home = crate::install::home_dir()?;
    let bin = std::env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("running executable path is not UTF-8"))?;
    let opts = crate::install::Options {
        dry_run,
        uninstall,
        only: agent.map(String::from),
        bin,
        no_skill,
        force,
        codex_home: std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from),
    };
    if !dry_run && !uninstall {
        crate::install::verify_binary(&opts.bin)?;
    }
    let actions = crate::install::apply(&home, &opts)?;

    print_install_actions(&actions, &out)?;
    if actions.iter().any(crate::install::Action::failed) {
        anyhow::bail!("agent setup failed; inspect the reported failures and rerun");
    }
    if !out.json && !dry_run && !uninstall {
        if actions.iter().all(|a| a.action == "skipped") {
            crate::output::stdout(format_args!("\nNo agents detected. Use `codeq install --agent <name>` to set one up explicitly.\n"));
        } else {
            crate::output::stdout(format_args!("\nRestart configured agents to load the MCP server and skill. Run `codeq doctor` to verify setup.\n"));
        }
    }
    Ok(())
}

pub fn cmd_doctor(agent: Option<&str>, out: Out) -> Result<()> {
    let home = crate::install::home_dir()?;
    let codex_home = std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from);
    let actions = crate::install::doctor(&home, agent, codex_home.as_deref())?;
    print_install_actions(&actions, &out)?;
    if actions.iter().any(crate::install::Action::failed) {
        anyhow::bail!("installation health checks failed");
    }
    Ok(())
}

fn print_install_actions(actions: &[crate::install::Action], out: &Out) -> Result<()> {
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
        crate::output::stdout(format_args!("{s}\n"));
    } else {
        for a in actions {
            let note = a
                .note
                .as_deref()
                .map(|n| format!("  ({n})"))
                .unwrap_or_default();
            crate::output::stdout(format_args!(
                "{:<12}  {:<6}  {:<15}  {}{}\n",
                a.agent, a.what, a.action, a.config, note
            ));
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
        "version": env!("CARGO_PKG_VERSION"),
        "build_revision": env!("CODEQ_BUILD_REVISION"),
        "guide_blake3": blake3::hash(crate::install::SKILL.as_bytes()).to_hex().to_string(),
        "result_schema": RESULT_SCHEMA,
        "index_schema": SCHEMA_VERSION,
        "configuration_keys": {"paths": "dotted; literal dots/brackets use JSON-quoted brackets; arrays use [index]", "max_depth": 64, "max_bytes": 1048576, "max_keys_per_file": 10000, "skip": ["package-lock.json", "composer.lock", "Cargo.lock", "poetry.lock", "pnpm-lock.yaml", "yarn.lock"], "aliases": "YAML aliases are not expanded"},
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
        // empty result on an unsupported format is distinguishable from a broken query.
        "data_only_languages": crate::index::NO_EXTRACTION,
        "fact_kinds": [
            "function", "method", "struct", "enum", "union", "trait", "module",
            "constant", "typealias", "macro", "class", "interface", "type", "import",
            "component", "field", "variant", "heading", "key", "element",
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
