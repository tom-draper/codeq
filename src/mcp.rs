//! A minimal MCP (Model Context Protocol) server over stdio.
//!
//! Speaks line-delimited JSON-RPC 2.0 on stdin/stdout and exposes codeq's query
//! commands as MCP tools, so an agent host can call codeq natively. Tool calls
//! use the same in-process CLI dispatcher and result rendering, with output
//! captured for the MCP response. A bounded worker pool leaves the input loop
//! available for discovery, ping and cancellation.

use std::io::{self, BufRead};
use std::path::Path;

use anyhow::Result;
use clap::Parser;
use serde_json::{json, Value};

mod executor;

pub(crate) use executor::{attach_connection, check_cancelled};

#[derive(Clone, Copy, Default, clap::ValueEnum)]
pub(crate) enum ToolProfile {
    Core,
    #[default]
    All,
}

impl ToolProfile {
    fn includes(self, name: &str) -> bool {
        matches!(self, Self::All) || CORE_TOOLS.contains(&name)
    }
}

const CORE_TOOLS: &[&str] = &[
    "codeq_map",
    "codeq_tree",
    "codeq_search",
    "codeq_symbols",
    "codeq_definitions",
    "codeq_explore",
    "codeq_read",
    "codeq_callers",
    "codeq_callees",
    "codeq_api",
    "codeq_deps",
    "codeq_tests",
];

pub fn serve(cwd: &Path, workers: u8, profile: ToolProfile) -> Result<()> {
    let stdin = io::stdin();
    let executor = executor::Executor::new(cwd, workers, profile);
    let mut input = stdin.lock();
    let mut buf = Vec::new();

    loop {
        buf.clear();
        // Bytes, not `lines()`: one non-UTF-8 byte from a client must not end
        // the whole session, and a line is decoded lossily like any other.
        if input.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(text) {
            Ok(value) => executor.submit(value),
            Err(error) => executor.parse_error(format!("parse error: {error}")),
        }
    }
    Ok(())
}

fn error_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The reply to one request object, or `None` for a notification (no id).
fn respond(req: &Value, cwd: &Path, profile: ToolProfile) -> Option<Value> {
    if !req.is_object() {
        return Some(error_reply(
            Value::Null,
            -32600,
            "invalid request: expected a JSON object".into(),
        ));
    }
    let id = match req.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => return None,
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params");

    let result = match method {
        "initialize" => Ok(initialize_result(params)),
        "tools/list" => Ok(json!({ "tools": tools(profile) })),
        "tools/call" => {
            let name = params.and_then(|p| p["name"].as_str()).unwrap_or("");
            if profile.includes(name) {
                Ok(call_tool(params, cwd))
            } else {
                Ok(tool_error(format!(
                    "tool '{name}' is not in the core profile; restart with codeq mcp --tools all"
                )))
            }
        }
        "ping" => Ok(json!({})),
        other => Err((-32601i64, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_reply(id, code, message),
    })
}

fn initialize_result(params: Option<&Value>) -> Value {
    // Echo the client's protocol version when given.
    let pv = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or("2024-11-05");
    json!({
        "protocolVersion": pv,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "codeq", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Use map/tree to orient, search with source to find implementations, explore for a symbol and its usage, and read with at or symbol for complete bodies. Scope common names with path. Call relationships are name-based; resolved uses import reachability, not type checking. Inspect notes and omitted results. The CLI command codeq guide prints this version's full agent instructions."
    })
}

/// Validate CLI arguments, then run the shared dispatcher with captured output.
fn call_tool(params: Option<&Value>, cwd: &Path) -> Value {
    let params = params.cloned().unwrap_or_else(|| json!({}));
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let cli_args = match build_args(name, &args) {
        Ok(a) => a,
        Err(e) => return tool_error(e),
    };

    let argv = std::iter::once("codeq".to_string()).chain(cli_args);
    let cli = match crate::Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => return tool_error(error.to_string()),
    };
    // The caller is a model paying per token, so it gets the CLI's compact text
    // rather than the JSON envelope: the same rows at roughly half the bytes.
    let out = cli.output_options();
    let (result, captured) = crate::output::capture(|| crate::dispatch(cwd, cli.command, out));
    match result {
        Ok(()) => {
            json!({ "content": [{ "type": "text", "text": tool_text(&captured.stdout, &captured.stderr) }] })
        }
        Err(error) => tool_error(format!(
            "codeq error: {}{}",
            format_args!("{error:#}"),
            if captured.stderr.is_empty() {
                String::new()
            } else {
                format!("\n{}", String::from_utf8_lossy(&captured.stderr).trim())
            }
        )),
    }
}

/// A successful call's text: the rows, then the notes. The CLI keeps notes on
/// stderr so a pipe sees only rows, but over MCP there is one channel, and a
/// note is what says a result stopped at the limit or why it is empty.
fn tool_text(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    let notes = String::from_utf8_lossy(stderr);
    let notes = notes.trim();
    if !notes.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(notes);
    }
    if text.trim().is_empty() {
        // An empty string reads as a tool that failed to answer.
        text = "(no results)".into();
    }
    text
}

fn tool_error(msg: String) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

// --- tool table, derived from the clap definitions ---------------------------
//
// The CLI is the one place an argument is declared. Each MCP tool is a clap
// subcommand: its input schema, its required list, and the argv it runs are all
// read from that subcommand's arguments, so a new flag appears over MCP without
// a second edit. What clap cannot say lives in the override table below, and the
// per-tool prose an agent reads is hand-written in `TOOL_DESCRIPTIONS`.

#[derive(Clone, Copy)]
struct ToolOverrides {
    /// CLI arguments MCP cannot meaningfully send. `stdin` needs a pipe, while
    /// `regexp` and `multiline` are compatibility aliases for search behavior
    /// that is already the default or is a no-op.
    hidden: &'static [&'static str],
    /// Global output flags this tool offers. Opt-in per tool, because a global
    /// on every tool is schema every session pays for: these go on the tools
    /// whose results are long row lists.
    globals: &'static [&'static str],
    /// Arguments MCP requires even though Clap leaves them optional.
    required: &'static [&'static str],
    /// Allowed values for otherwise free-form string arguments.
    choices: &'static [(&'static str, &'static [&'static str])],
    /// Groups of which exactly one argument is sent, with the first winning.
    one_of: &'static [&'static str],
}

const EMPTY_TOOL_OVERRIDES: ToolOverrides = ToolOverrides {
    hidden: &[],
    globals: &[],
    required: &[],
    choices: &[],
    one_of: &[],
};

/// The flags that shrink a long row list: fewer columns, the path hoisted onto
/// a header, a cap on rows per file.
const ROW_GLOBALS: &[&str] = &["fields", "group", "max_per_file"];

/// MCP-specific behavior that Clap cannot express. Keeping each command's
/// exceptions together makes the remaining adapter metadata reviewable.
const TOOL_OVERRIDES: &[(&str, ToolOverrides)] = &[
    (
        "astgrep",
        ToolOverrides {
            globals: ROW_GLOBALS,
            ..EMPTY_TOOL_OVERRIDES
        },
    ),
    (
        "explore",
        ToolOverrides {
            one_of: &["at", "name"],
            ..EMPTY_TOOL_OVERRIDES
        },
    ),
    (
        "read",
        ToolOverrides {
            one_of: &["at", "file"],
            ..EMPTY_TOOL_OVERRIDES
        },
    ),
    (
        "references",
        ToolOverrides {
            globals: ROW_GLOBALS,
            ..EMPTY_TOOL_OVERRIDES
        },
    ),
    (
        "search",
        ToolOverrides {
            // `regexp` is `pattern` again (an alternation says the same) and
            // `multiline` is a no-op kept for grep's `-U`.
            hidden: &["stdin", "regexp", "multiline"],
            globals: ROW_GLOBALS,
            required: &["pattern"],
            choices: &[("in", &["code", "comments", "strings"])],
            ..EMPTY_TOOL_OVERRIDES
        },
    ),
];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Int,
    Bool,
    /// Repeatable string option: an array, or one string.
    List,
}

struct Param {
    /// The MCP argument name: the option's long name with `_`, or the
    /// positional's id.
    key: String,
    /// `--long`, or `None` for a positional.
    flag: Option<String>,
    kind: Kind,
    required: bool,
    choices: &'static [&'static str],
}

struct ToolDef {
    name: &'static str,
    command: String,
    description: &'static str,
    params: Vec<Param>,
    one_of: &'static [&'static str],
}

fn table_entry<'a, T>(table: &'a [(&str, T)], command: &str) -> Option<&'a T> {
    table.iter().find(|(c, _)| *c == command).map(|(_, v)| v)
}

fn kind_of(arg: &clap::Arg) -> Kind {
    use clap::ArgAction;
    match arg.get_action() {
        ArgAction::SetTrue => Kind::Bool,
        ArgAction::Append => Kind::List,
        // Compared as type ids, not by debug name: clap only keeps the name in
        // debug builds, so matching on it made every integer a string in the
        // release binary while the tests, built in debug, saw integers.
        _ => {
            use std::any::TypeId;
            let id = arg.get_value_parser().type_id();
            let ints = [
                TypeId::of::<usize>(),
                TypeId::of::<u64>(),
                TypeId::of::<i64>(),
            ];
            if ints.iter().any(|t| id == *t) {
                Kind::Int
            } else {
                Kind::Text
            }
        }
    }
}

fn tool_def(name: &'static str, description: &'static str, root: &clap::Command) -> ToolDef {
    let command = name.strip_prefix("codeq_").unwrap_or(name);
    let sub = root
        .get_subcommands()
        .find(|c| c.get_name() == command)
        .unwrap_or_else(|| panic!("MCP tool {name} has no CLI subcommand"));
    let overrides = table_entry(TOOL_OVERRIDES, command)
        .copied()
        .unwrap_or(EMPTY_TOOL_OVERRIDES);
    let params = sub
        .get_arguments()
        .filter(|a| !a.is_global_set() && !overrides.hidden.contains(&a.get_id().as_str()))
        .chain(
            root.get_arguments()
                .filter(|a| overrides.globals.contains(&a.get_id().as_str())),
        )
        .map(|a| {
            let id = a.get_id().as_str();
            let key = match a.get_long() {
                Some(long) => long.replace('-', "_"),
                None => id.to_string(),
            };
            Param {
                flag: a.get_long().map(|l| format!("--{l}")),
                kind: kind_of(a),
                required: (a.is_required_set() || overrides.required.contains(&key.as_str()))
                    && !overrides.one_of.contains(&key.as_str()),
                choices: overrides
                    .choices
                    .iter()
                    .find(|(choice_key, _)| *choice_key == key)
                    .map_or(&[], |(_, choices)| *choices),
                key,
            }
        })
        .collect();
    ToolDef {
        name,
        command: sub.get_name().to_string(),
        description,
        params,
        one_of: overrides.one_of,
    }
}

/// Every MCP tool, in the order `tools/list` presents them.
fn tool_defs() -> &'static [ToolDef] {
    static DEFS: std::sync::OnceLock<Vec<ToolDef>> = std::sync::OnceLock::new();
    DEFS.get_or_init(|| {
        let root = <crate::Cli as clap::CommandFactory>::command();
        TOOL_DESCRIPTIONS
            .iter()
            .map(|(name, desc)| tool_def(name, desc, &root))
            .collect()
    })
}

fn param_schema(p: &Param) -> Value {
    match p.kind {
        Kind::Text if !p.choices.is_empty() => json!({ "type": "string", "enum": p.choices }),
        Kind::Text => json!({ "type": "string" }),
        Kind::Int => json!({ "type": "integer" }),
        Kind::Bool => json!({ "type": "boolean" }),
        Kind::List => json!({ "type": "array", "items": { "type": "string" } }),
    }
}

/// Tool definitions with JSON Schemas, returned by `tools/list`.
fn tools(profile: ToolProfile) -> Value {
    let list: Vec<Value> = tool_defs()
        .iter()
        .filter(|t| profile.includes(t.name))
        .map(|t| {
            let props: serde_json::Map<String, Value> = t
                .params
                .iter()
                .map(|p| (p.key.clone(), param_schema(p)))
                .collect();
            let required: Vec<&str> = t
                .params
                .iter()
                .filter(|p| p.required)
                .map(|p| p.key.as_str())
                .collect();
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": { "type": "object", "properties": props, "required": required }
            })
        })
        .collect();
    Value::Array(list)
}

/// Map an MCP tool name + arguments to codeq CLI arguments, or say what is
/// wrong with them.
///
/// Every argument must be one the tool declares and of the declared type. An
/// unknown key used to be dropped: `paths: ["src/a.rs"]` on a tool without
/// `paths` ran the search repo-wide and returned it as the answer to a scoped
/// question. A filter that silently does not apply is worse than an error.
fn build_args(tool: &str, a: &Value) -> Result<Vec<String>, String> {
    let def = tool_defs()
        .iter()
        .find(|t| t.name == tool)
        .ok_or_else(|| format!("unknown tool: {tool}"))?;
    let empty = serde_json::Map::new();
    let given = match a {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(format!("{tool}: arguments must be an object")),
    };
    let unknown: Vec<&str> = given
        .keys()
        .map(String::as_str)
        .filter(|k| !def.params.iter().any(|p| p.key == *k))
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = def.params.iter().map(|p| p.key.as_str()).collect();
        return Err(format!(
            "{tool}: unknown argument(s) {}; it takes: {}",
            unknown.join(", "),
            known.join(", ")
        ));
    }
    // Flags accumulate in `args`; caller-supplied *values* go to `pos` and are
    // emitted after a `--` separator. Without it a value that happens to start
    // with a dash is read as a flag by the CLI's parser: `pattern: "--json"` —
    // an entirely ordinary thing to search a codebase for — failed as "required
    // argument not provided", and `symbol: "--help"` printed the help page and
    // exited 0, handing an agent the help text as a successful answer.
    let mut args: Vec<String> = vec![def.command.clone()];
    let mut pos: Vec<String> = Vec::new();
    let present = |k: &str| given.get(k).is_some_and(|v| !v.is_null());
    let chosen = if def.one_of.is_empty() {
        None
    } else {
        Some(
            *def.one_of
                .iter()
                .find(|k| present(k))
                .ok_or_else(|| format!("{tool}: one of {} is required", def.one_of.join(", ")))?,
        )
    };
    // A scalar as the CLI wants it. A number where a string is declared (or the
    // reverse) is accepted: clients disagree about which to send, and the CLI
    // parses the text either way.
    let scalar = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    for p in &def.params {
        if def.one_of.contains(&p.key.as_str()) && chosen != Some(p.key.as_str()) {
            continue;
        }
        let Some(v) = given.get(&p.key).filter(|v| !v.is_null()) else {
            if p.required {
                return Err(format!("{tool}: missing required argument: {}", p.key));
            }
            continue;
        };
        let wrong = |want: &str| format!("{tool}: {} must be {want}, got {v}", p.key);
        let values = match p.kind {
            Kind::Bool => {
                if v.as_bool().ok_or_else(|| wrong("a boolean"))? {
                    args.extend(p.flag.clone());
                }
                continue;
            }
            Kind::Int => {
                let n = scalar(v).filter(|s| s.parse::<i64>().is_ok());
                vec![n.ok_or_else(|| wrong("an integer"))?]
            }
            Kind::Text => vec![scalar(v).ok_or_else(|| wrong("a string"))?],
            Kind::List => match v {
                Value::Array(items) => items
                    .iter()
                    .map(scalar)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| wrong("an array of strings"))?,
                one => vec![scalar(one).ok_or_else(|| wrong("an array of strings"))?],
            },
        };
        match &p.flag {
            Some(flag) => {
                for v in values {
                    args.push(flag.clone());
                    args.push(v);
                }
            }
            None => pos.extend(values),
        }
    }
    // `--` ends flag parsing, so every caller-supplied value after it is taken
    // literally however it starts.
    if !pos.is_empty() {
        args.push("--".into());
        args.extend(pos);
    }
    Ok(args)
}

/// What each tool tells an agent about itself. Hand-written: this is the prose
/// that decides when an agent reaches for the tool, so it is tuned, not derived.
/// The order here is the order of `tools/list`.
const TOOL_DESCRIPTIONS: &[(&str, &str)] = &[
    ("codeq_packages", "Join manifest dependencies to import sites in npm, Cargo, Python project and Go manifests. name selects a package; undeclared and unimported filter declaration/import mismatches. Import matching is labeled exact or heuristic; unresolved imports and possible standard libraries remain explicit. limit caps rows."),
    ("codeq_discover", "Find definitions when the name is unknown. query splits identifiers and ranks word matches across names, signatures, docs and paths. Each candidate includes its score, match reasons and exact range. source adds candidate bodies under max_tokens. limit bounds the ranked candidates."),
    (
        "codeq_explore",
        "Symbol, callers, callees, imports and source in one call. Give name or at=path:line; path scopes names. max_tokens budgets source; max_response_tokens budgets the reply with cursor continuation. Calls are name-based; resolved filters imports. Ambiguous callee bodies need include_ambiguous.",
    ),
    (
        "codeq_map",
        "Orient in the repo: factual counts by language, directory, imported module, and file. Start here in an unfamiliar codebase.",
    ),
    (
        "codeq_search",
        "Search regex or fixed text with enclosing definitions. symbols returns ranked definitions; source includes their bodies under max_tokens. Scope with paths/path/language/glob. within selects one exact definition; within_at selects its position and rejects ambiguous locations. in selects code/comments/strings; Markdown fences are strings. limit bounds hits; group and max_per_file summarize files. Explicit unindexed paths are searched from disk.",
    ),
    (
        "codeq_symbols",
        "List defined symbols (functions, classes, structs, interfaces, …), optionally filtered by name substring, kind, language, or path. Set signatures to include each declaration.",
    ),
    (
        "codeq_definitions",
        "Exact symbol definitions with signatures and ranges. symbol accepts one name or an array; path disambiguates. limit applies per name. Use symbols for unknown spellings, read/explore for bodies.",
    ),
    (
        "codeq_references",
        "Textual identifier occurrences with syntax-backed use_kind and evidence. kind filters call/read/write/read-write/type-use/import/decl/unknown; read and write include read-write. Unsupported syntax stays unknown. within/within_at scope to one exact definition range. calls selects syntax call sites; on matches a literal receiver. resolved filters import reachability without type checking. For distinct enclosing definitions use callers.",
    ),
    (
        "codeq_callers",
        "Definitions referencing a name, with tests flagged. calls selects syntax calls; examples includes representative calls; on matches literal receivers. path filters callers. depth follows names transitively; resolved filters direct callers by imports. Not type-resolved.",
    ),
    (
        "codeq_callees",
        "Call targets inside a named definition; path scopes the definition. examples includes call syntax. Resolution is qualified, name-only or ambiguous, without type checking. depth follows names transitively; resolved filters direct callees by imports and may omit unresolved imports.",
    ),
    (
        "codeq_about",
        "Definitions, members, callers and callees without bodies. name accepts one name or an array; path scopes definitions and callees. Calls are name-based; resolved filters imports. Use explore for source.",
    ),
    (
        "codeq_callpath",
        "Shortest name-based call chain from one symbol to another within max_depth. path scopes the start. Empty results do not prove runtime unreachability.",
    ),
    (
        "codeq_untested",
        "Public definitions without textual test references. Heuristic candidate list; indirect tests can be missed. path and kind scope the reported API.",
    ),
    (
        "codeq_members",
        "Fields, variants and methods structurally nested in a type. path disambiguates; kind filters members. Separate implementation blocks, including Rust impl methods, are outside the type body.",
    ),
    (
        "codeq_outline",
        "The structure of a file top to bottom: its definitions with signatures and nesting depth. Works on markdown too, where a heading is a definition — so this is a document's table of contents. docs=true adds each definition's doc comment (or a section's first line).",
    ),
    (
        "codeq_at",
        "Resolve a location (e.g. from a stack trace or compiler error) to the definition(s) enclosing it, innermost first. Location is <path>:<line>.",
    ),
    (
        "codeq_read",
        "Complete definitions at one or several at=path:line positions, or file plus symbol. file accepts arrays and inline ranges; lines selects known ranges. File-only reads fold long bodies; full expands them. raw returns exact bytes for one block. Markdown headings are symbols.",
    ),
    (
        "codeq_imports",
        "List import statements (raw module strings), optionally scoped to files whose path matches.",
    ),
    (
        "codeq_importers",
        "Files whose imports match a module string.",
    ),
    (
        "codeq_implementors",
        "Types with explicit implements/extends/schema references edges pointing to a name. relation filters edge kinds. Syntax-based and name-matched; implicit structural implementations are not inferred.",
    ),
    (
        "codeq_supertypes",
        "Explicit base types, interfaces or referenced tables for a name. relation filters implements/extends/references. Names are as written, without transitive type resolution.",
    ),
    (
        "codeq_unused",
        "Candidate definitions with no uses outside their declarations. Comments do not count; strings do. Same-named definitions can mask unused code. Framework registrations and trait contracts are excluded. include_tests adds tests. path/kind scope reports, not usage checks.",
    ),
    (
        "codeq_entrypoints",
        "Framework definitions registered by decorator syntax, with the observed registration text. include_tests adds structurally identified tests. Does not infer every runtime entry point.",
    ),
    (
        "codeq_tree",
        "The repo's directory SHAPE, folded to a depth, with each directory's file and symbol counts aggregated from everything below it. Use this to orient in an unfamiliar repo: unlike tree(1) it is ignore-aware (no node_modules/.git noise) and reports where the *code* is rather than just where files are; unlike codeq_map it keeps the nesting instead of collapsing to top-level directories. min_symbols=1 jumps straight to the directories that hold code, which is the fastest way to find the source in a data- or asset-heavy repo. depth defaults to 3; counts still include everything below the fold, so folding summarises rather than truncates.",
    ),
    (
        "codeq_hotspots",
        "Definitions ranked by name-reference count. calls counts syntax calls only. Same-named definitions share counts; use callers to investigate. path/kind/language scope reports, not counts.",
    ),
    (
        "codeq_api",
        "Public definitions and signatures in a file/directory. compare_to instead reports API changes against a Git ref. Visibility follows each language; kind filters declarations.",
    ),
    (
        "codeq_deps",
        "Internal import dependencies of a file, transitive to depth. cycles lists repo-wide cycles; external lists unresolved/external imports. Project-aware where supported, heuristic otherwise. Give file, cycles or external. Unresolved does not mean unused.",
    ),
    (
        "codeq_astgrep",
        "Match syntax with a pattern and language. $NAME captures a node; repeated names must agree. source includes enclosing bodies under max_tokens. Use explain=path:line instead of pattern to inspect syntax before constructing a pattern.",
    ),
    (
        "codeq_clones",
        "Definitions with identical syntax shape up to renaming. Exact matches, not fuzzy similarity. min_nodes filters small definitions; include_tests adds tests. Inspect before deduplicating.",
    ),
    (
        "codeq_tests",
        "Which tests exercise a symbol: the test functions that reference the given name (heuristic — a test-file path or test-like function name). Great for the edit-then-find-the-test loop.",
    ),
    (
        "codeq_changed",
        "Files changed vs a git ref (default main); set symbols to get the definitions the change touched instead.",
    ),
    (
        "codeq_affected",
        "Changed definitions, transitive name-based callers and candidate tests against compare_to. depth bounds traversal; limit bounds output. An incomplete impact estimate, not proof of every runtime effect.",
    ),
    (
        "codeq_context",
        "Source for symbols or changed definitions under max_tokens. path disambiguates; expand adds one reference hop; resolved filters expansion by imports. no_text returns metadata; plan explains selection. Inspect omitted/reduced chunks.",
    ),
    (
        "codeq_files",
        "List indexed files, optionally filtered by path substring or language.",
    ),
    (
        "codeq_capabilities",
        "Describe what codeq supports: commands, languages, fact kinds, schema, and language_support — a per-language matrix of which extractions exist (definitions, imports, members, relations, spans, calls). Check it before reading an empty result as evidence of absence: Go, C, and Zig have no extracted relation edges, Ruby declares no fields, and several languages have no import syntax codeq reads.",
    ),
    (
        "codeq_guide",
        "Read this executable's bundled agent guide, including search-to-source workflows and supported flags. Use when installed instructions may be stale. Optional check compares a skill file with the bundled guide without changing it. Requires no repository or index.",
    ),
    (
        "codeq_graph",
        "All explicit type/schema relation edges. Filter relation/path/language; mermaid emits a graph document. Calls are excluded; use callers/callees.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool whose primary value is a positional must put it after `--`, so a
    /// value that starts with a dash is read as a literal and not as a flag.
    #[test]
    fn positionals_follow_the_separator() {
        for (tool, key) in [
            ("codeq_members", "name"),
            ("codeq_untested", "path"),
            ("codeq_deps", "file"),
        ] {
            let args = build_args(tool, &json!({ key: "--help" })).unwrap();
            let sep = args.iter().position(|a| a == "--").expect(tool);
            assert_eq!(args[sep + 1..], ["--help"], "{tool}: {args:?}");
            assert!(
                !args[..sep].contains(&"--help".to_string()),
                "{tool}: {args:?}"
            );
        }
    }

    /// `clones` takes its path as a positional; sending `--path` made every
    /// scoped call fail as an unknown argument.
    #[test]
    fn clones_path_is_positional() {
        let args = build_args(
            "codeq_clones",
            &json!({ "path": "src", "kind": "function" }),
        );
        assert_eq!(args.unwrap(), ["clones", "--kind", "function", "--", "src"]);
    }

    /// The tool list and the CLI cannot drift: every query subcommand has a
    /// description and every description names a subcommand (`tool_defs` panics
    /// on the latter).
    #[test]
    fn every_query_subcommand_is_a_tool() {
        let root = <crate::Cli as clap::CommandFactory>::command();
        let mut cli: Vec<String> = root
            .get_subcommands()
            .map(|c| format!("codeq_{}", c.get_name()))
            .filter(|n| {
                ![
                    "codeq_index",
                    "codeq_install",
                    "codeq_uninstall",
                    "codeq_doctor",
                    "codeq_mcp",
                    "codeq_help",
                ]
                .contains(&n.as_str())
            })
            .collect();
        let mut tools: Vec<String> = tool_defs().iter().map(|t| t.name.to_string()).collect();
        cli.sort();
        tools.sort();
        assert_eq!(tools, cli);
    }

    #[test]
    fn every_override_names_a_cli_subcommand() {
        let root = <crate::Cli as clap::CommandFactory>::command();
        for (command, _) in TOOL_OVERRIDES {
            assert!(
                root.get_subcommands()
                    .any(|subcommand| subcommand.get_name() == *command),
                "override names an unknown subcommand: {command}"
            );
        }
    }

    #[test]
    fn only_documented_search_options_are_omitted_from_mcp() {
        let hidden: Vec<(&str, &str)> = TOOL_OVERRIDES
            .iter()
            .flat_map(|(command, overrides)| {
                overrides
                    .hidden
                    .iter()
                    .map(move |argument| (*command, *argument))
            })
            .collect();
        assert_eq!(
            hidden,
            [
                ("search", "stdin"),
                ("search", "regexp"),
                ("search", "multiline")
            ]
        );
    }

    /// An explore target is a name or a position, `at` winning; neither is invalid.
    #[test]
    fn explore_takes_at_or_name() {
        let both = build_args("codeq_explore", &json!({ "name": "f", "at": "a.rs:3" })).unwrap();
        assert_eq!(both, ["explore", "--at", "a.rs:3"]);
        let name = build_args("codeq_explore", &json!({ "name": "f" })).unwrap();
        assert_eq!(name, ["explore", "--", "f"]);
        let expanded = build_args(
            "codeq_explore",
            &json!({ "name": "f", "include_ambiguous": true }),
        )
        .unwrap();
        assert_eq!(expanded, ["explore", "--include-ambiguous", "--", "f"]);
        let paged = build_args(
            "codeq_explore",
            &json!({ "name": "f", "max_response_tokens": 512, "cursor": "v1:hash:1:2:3:4:5" }),
        )
        .unwrap();
        assert_eq!(
            paged,
            [
                "explore",
                "--max-response-tokens",
                "512",
                "--cursor",
                "v1:hash:1:2:3:4:5",
                "--",
                "f"
            ]
        );
        assert!(build_args("codeq_explore", &json!({})).is_err());
    }

    #[test]
    fn mcp_exposes_precision_controls_and_ast_explanations() {
        let listed = tools(ToolProfile::All);
        let tool = |name: &str| {
            listed
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()
        };
        for (name, params) in [
            ("codeq_about", &["resolved"][..]),
            (
                "codeq_explore",
                &["resolved", "max_response_tokens", "cursor"][..],
            ),
            ("codeq_callers", &["limit"][..]),
            ("codeq_callees", &["limit"][..]),
            ("codeq_astgrep", &["explain"][..]),
            ("codeq_search", &["within", "within_at"][..]),
            ("codeq_references", &["within", "within_at", "kind"][..]),
            ("codeq_deps", &["reverse", "by", "limit"][..]),
            ("codeq_discover", &["query", "source", "limit"][..]),
            ("codeq_packages", &["name", "limit"][..]),
        ] {
            for param in params {
                assert!(
                    tool(name)["inputSchema"]["properties"].get(param).is_some(),
                    "{name} does not expose {param}"
                );
            }
        }
        assert_eq!(
            build_args("codeq_astgrep", &json!({ "explain": "src/lib.rs:12" })).unwrap(),
            ["astgrep", "--explain", "src/lib.rs:12"]
        );
        assert_eq!(
            build_args("codeq_callees", &json!({ "name": "run", "limit": 7 })).unwrap(),
            ["callees", "--limit", "7", "--", "run"]
        );
    }

    #[test]
    fn read_takes_positions_or_files() {
        let args = build_args("codeq_read", &json!({ "at": ["a.rs:3", "b.rs:7"] })).unwrap();
        assert_eq!(args, ["read", "--at", "a.rs:3", "--at", "b.rs:7"]);
        let args = build_args("codeq_read", &json!({ "file": "a.rs", "symbol": "f" })).unwrap();
        assert_eq!(args, ["read", "--symbol", "f", "--", "a.rs"]);
        assert!(build_args("codeq_read", &json!({})).is_err());
    }
}
