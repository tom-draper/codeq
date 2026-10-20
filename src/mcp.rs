//! A minimal MCP (Model Context Protocol) server over stdio.
//!
//! Speaks line-delimited JSON-RPC 2.0 on stdin/stdout and exposes codeq's query
//! commands as MCP tools, so an agent host (Claude Code, Cursor, …) can call
//! codeq natively instead of shelling out. Each tool call is bridged to the
//! codeq CLI as a subprocess in the server's working directory, so the tool
//! output is exactly the CLI's JSON. Deliberately dependency-free: no async
//! runtime, no SDK.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde_json::{json, Value};

pub fn serve() -> Result<()> {
    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
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
        let reply = match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(batch)) if batch.is_empty() => Some(error_reply(
                Value::Null,
                -32600,
                "invalid request: empty batch".into(),
            )),
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch
                    .iter()
                    .filter_map(|req| respond(req, &exe, &cwd))
                    .collect();
                // A batch of notifications gets no reply at all.
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(req) => respond(&req, &exe, &cwd),
            // A client waiting on a reply to a line that never parsed would
            // wait forever; JSON-RPC defines the answer, with a null id.
            Err(e) => Some(error_reply(
                Value::Null,
                -32700,
                format!("parse error: {e}"),
            )),
        };
        if let Some(msg) = reply {
            writeln!(out, "{}", serde_json::to_string(&msg)?)?;
            out.flush()?;
        }
    }
    Ok(())
}

fn error_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The reply to one request object, or `None` for a notification (no id).
fn respond(req: &Value, exe: &Path, cwd: &Path) -> Option<Value> {
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
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(call_tool(params, exe, cwd)),
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
        "serverInfo": { "name": "codeq", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// Run the tool by bridging to the codeq CLI. Returns an MCP tool result.
fn call_tool(params: Option<&Value>, exe: &Path, cwd: &Path) -> Value {
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

    // The caller is a model paying per token, so it gets the CLI's compact text
    // rather than the JSON envelope: the same rows at roughly half the bytes.
    match Command::new(exe).args(&cli_args).current_dir(cwd).output() {
        Ok(o) if o.status.success() => {
            json!({ "content": [{ "type": "text", "text": tool_text(&o.stdout, &o.stderr) }] })
        }
        Ok(o) => tool_error(format!(
            "codeq error: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => tool_error(format!("failed to run codeq: {e}")),
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
fn tools() -> Value {
    let list: Vec<Value> = tool_defs()
        .iter()
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
    (
        "codeq_explore",
        "Understand a symbol in one call: its definition, callers, callees, and source. name is the exact symbol; path disambiguates a common name; resolved prunes unrelated callers and callees; max_tokens caps source. Ambiguous callees are listed but their source is deferred unless include_ambiguous=true. Give either name or at. at is a '<path>:<line>' position and explores the exact enclosing definition.",
    ),
    (
        "codeq_map",
        "Orient in the repo: factual counts by language, directory, imported module, and file. Start here in an unfamiliar codebase.",
    ),
    (
        "codeq_search",
        "Search file contents. Regex by default (same engine as ripgrep); set fixed for a literal. Each match is annotated with its enclosing definition. Set symbols to rank definitions by matching-line count (descending; ties by path and position), or source to read their bodies once under max_tokens in that rank order, avoiding a search-then-read loop. Superset of grep: -A/-B/-C context, files_with_matches, count, and 'in' (code|comments|strings) which grep can't do. paths scopes it like grep's positional paths, and a path the index does not hold (gitignored output, a log, a file outside the repo) is searched from disk. On a broad pattern, max_per_file with group returns the shape of the result for a fraction of the tokens. In markdown, fenced code blocks count as strings and prose as comments, so 'in' separates documented examples from the text around them.",
    ),
    (
        "codeq_symbols",
        "List defined symbols (functions, classes, structs, interfaces, …), optionally filtered by name substring, kind, language, or path. Set signatures to include each declaration.",
    ),
    (
        "codeq_definitions",
        "Locate definitions of exact symbol names, with signature, doc, and precise byte/line ranges. symbol accepts one name or an array. Batches return named results in argument order, with a note for each missing name. path scopes to matching files; limit applies per name.",
    ),
    (
        "codeq_references",
        "Word-boundary uses of a symbol name across the repo, each annotated with its enclosing definition; definition sites are flagged. Textual (not type-resolved) by default. resolved=true prunes references whose file can't reach a definition through imports. calls=true keeps only real call sites and on='RECV' keeps only calls on that receiver; both are read from the parse tree, so a declaration, a mention in a string or comment, and formatting (a wrapped method chain) are all handled correctly. The receiver is the expression as written, so 'db' does not match mydb.query() or a.inner.query() — use on='a.inner', or even on='conn()'; PHP receivers keep their sigil (on='$db'). Precision filters compose.",
    ),
    (
        "codeq_callers",
        "The distinct definitions that reference a symbol (its callers), with test sites flagged — what could break if you change it. Textual, not type-resolved. path restricts to callers in matching files. depth (default 1) walks transitively up the call tree, each result tagged with its hop distance. Precision filters (direct callers only): resolved=true prunes callers whose file can't reach a definition through imports; calls=true keeps only real call sites; on='RECV' keeps only calls on that receiver. Both come from the parse tree, so declarations, prose mentions and line breaks are handled; the receiver is the expression as written (on='a.inner', on='conn()', PHP on='$db'). examples=true surfaces the actual call expression at each site (one representative per caller) — how the symbol is really invoked; for every call site use codeq_references with calls=true.",
    ),
    (
        "codeq_callees",
        "The definitions referenced by name inside a symbol's body (its callees) — what it depends on. Already call-position only: a bare mention isn't a callee, and neither is helper() written inside a string or a comment. path scopes to the symbol's definition(s). depth (default 1) walks transitively down the call tree. resolved=true (direct callees only) keeps only callees whose definition file the symbol's own file can reach through imports, pruning unrelated same-named definitions in unreachable files. examples=true adds `call` — the call as written, with its receiver (`subprocess.run`, not `run`) — and `call_line`; use it to see at a glance whether a row is a real callee or a name collision, since a module-qualified call is attributed to every definition sharing the bare name.",
    ),
    (
        "codeq_about",
        "One-call dossier before editing symbols, WITHOUT source. name accepts one name or an array; batches return named results in argument order with a note for each missing name. Each dossier contains: its definition(s) with signature and doc comment, its callers (who breaks, with test call sites flagged), and its callees (what it depends on). Replaces several separate lookups. path scopes the definition and callees to matching files to disambiguate a common name. Same neighbourhood as codeq_explore, which adds the verbatim source but trims the per-node metadata. Reach for codeq_explore when you intend to change the code, and this when you want the metadata or a cheap 'where does this live'.",
    ),
    (
        "codeq_callpath",
        "The shortest CALL chain from one symbol to another (from calls … calls to), if one exists — reachability with the route shown, e.g. how an entry point reaches a low-level function. Nothing to do with file paths. Name-based; empty when unreachable within max_depth (default 8). path scopes the start symbol to matching files, to disambiguate a common/cross-language `from` like `main`.",
    ),
    (
        "codeq_untested",
        "Exported definitions that no test refers to — api composed with tests, which otherwise costs one call per public symbol. Public only (a private helper is usually covered through the exported thing that calls it). Heuristic in the same way tests is, so it over-reports coverage rather than under-report it: a test that reaches the code without naming it is invisible.",
    ),
    (
        "codeq_members",
        "What a type contains: its fields, enum variants, and methods, in source order. Use it to learn a struct/class/enum's shape before constructing or destructuring it, instead of reading the file. Unlike the call-graph commands this is structural, not name-based: a member is a definition nested directly inside the type's byte range, so there are no same-name false positives. kind filters to one of field/variant/method/function; path disambiguates a common type name. Languages that don't declare members (Ruby attr_accessor, Python self.x in __init__) return nothing rather than a guess, and in Rust a struct's methods live in a separate impl block so only its fields are returned.",
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
        "Read repository source. Choose at for complete definitions enclosing search hits or stack frames (one or several path:line positions; overlapping source once), or file with symbol for a named definition. These use exact boundaries instead of guessed sed windows. With file alone, long bodies fold to signatures and expandable line ranges; imports stay visible. For known slices, file accepts [\"a.rs:40-80\", \"b.rs:12\"] or use lines with one file. raw emits source bytes without line numbers, for edit anchors; it requires one block. full unfolds a file. Markdown headings work as symbols. Provide either at or file.",
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
        "Types that implement or extend a trait / interface / base class — the inheritance edges the call graph doesn't cover (impl T for X in Rust, extends/implements in Java/TS/PHP, class X(Base) in Python, class X < Y in Ruby, base lists in C++/C#). Use it to answer 'who implements this interface' before changing it. Go (structural typing), C, and Zig have no extracted relation edges. Name-based. relation filters to 'implements' or 'extends'.",
    ),
    (
        "codeq_supertypes",
        "The traits / interfaces / base classes a type extends or implements — the reverse of implementors (what does X derive from). The names as written at the declaration, not transitively resolved. Name-based. relation filters to 'implements' or 'extends'.",
    ),
    (
        "codeq_unused",
        "Definitions with no reference anywhere outside their own definition site — candidate dead code. A mention in a comment is not a use, so a stale TODO cannot mask dead code; a mention in a string still is, because reflection, serde renames, config keys and SQL identifiers are real uses. Name-based, so a same-named symbol elsewhere masks it; it under-reports rather than misleads. visibility is shown so a public symbol (maybe external API, not dead) is distinguishable from a private one. Test-file definitions are excluded by default (framework entry points look unused); set include_tests to include them. path/kind scope what is reported; the usage check is always repo-wide.",
    ),
    (
        "codeq_entrypoints",
        "Definitions explicitly registered as framework entry points (routes, commands, plugin hooks) by decorator syntax. registration is the exact decorator observed, not an inferred framework. Tests are excluded by default; set include_tests to list definitions structurally marked as tests too.",
    ),
    (
        "codeq_tree",
        "The repo's directory SHAPE, folded to a depth, with each directory's file and symbol counts aggregated from everything below it. Use this to orient in an unfamiliar repo: unlike tree(1) it is ignore-aware (no node_modules/.git noise) and reports where the *code* is rather than just where files are; unlike codeq_map it keeps the nesting instead of collapsing to top-level directories. min_symbols=1 jumps straight to the directories that hold code, which is the fastest way to find the source in a data- or asset-heavy repo. depth defaults to 3; counts still include everything below the fold, so folding summarises rather than truncates.",
    ),
    (
        "codeq_hotspots",
        "Definitions ranked by how often their name is referenced in code — the inverse of codeq_unused, and the fastest way to find what is load-bearing in a file or repo you have just met. One row per name (the count is a property of the name), with defs telling you how many definitions share it, and path/line pointing at the first. Set calls=true to count only call sites, which is what you usually want for functions: without it a name that is also a common variable scores high. Name-based, so a short common name (new, get, path) ranks high because many unrelated symbols spell it the same way — treat a row as 'this identifier carries weight' and use codeq_callers with calls=true for the precise question. path/kind/language scope what is reported; the count is always repo-wide.",
    ),
    (
        "codeq_api",
        "The public API surface of a file or directory. compare_to=<git ref> instead reports how that surface differs from the ref — what was added, removed, or changed shape — which is the review question a raw diff cannot answer, since most edited lines are internal. Otherwise: the definitions it exports for outside use, with signatures. \"Public\" is each language's own notion (Rust pub, JS/TS export, Go capitalization, Python non-underscore, a public modifier in Java/C#/PHP, non-static in C/C++); pub(crate)/protected are excluded. Use it to learn how to call an unfamiliar module without reading whole files. path scopes to a file/dir substring; kind filters to one kind (function, struct, class, …).",
    ),
    (
        "codeq_deps",
        "Internal files a file depends on, resolved from imports. TypeScript paths aliases, npm workspace packages, and Go module packages use project configuration; relative imports use their written paths. Other languages use deterministic path matching. Pass file, cycles=true, or external=true. depth (default 1) walks transitive dependencies. external=true lists third-party imports and broken local paths. Project-style imports that could not be resolved include a resolution_reason.",
    ),
    (
        "codeq_astgrep",
        "Structural AST search using the language grammar. Supply pattern and language to find code by shape, or explain='<file>:<line>' to inspect node kinds and fields at a location before writing a pattern. A repeated metavariable must bind identically. source=true returns each matching definition once under max_tokens; limit counts definitions in source mode.",
    ),
    (
        "codeq_clones",
        "Definitions whose syntax shape is identical up to renaming — the same code written twice. Exact, not fuzzy: shapes either match or they don't, so there is no similarity score to interpret and no threshold to tune. Rows are grouped by a shared shape hash, biggest duplication first. Use it before writing a helper (is this already implemented?) and to find the copy-paste that needs the same fix in several places. min_nodes (default 24) sets how much code a shared shape must cover; kind narrows to e.g. function.",
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
        "For a diff (vs compare_to, default main): the definitions it touched, their transitive callers up to depth hops (the impacted set), and the tests that exercise any of them — 'what should I re-check and re-run for this change'. Returns changed, impacted, and tests. Name-based like the rest of the graph.",
    ),
    (
        "codeq_context",
        "Assemble token-budgeted code context from explicit seeds: named symbols and/or changed files. path scopes the seeds to matching files. Set expand for one deterministic hop to referenced definitions; no_text returns provenance only (locations without source, for a cheap map).",
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
        "Every explicit type or schema relation in the repository as an edge list. relation filters to implements, extends, or references; mermaid emits a graph LR document. This is not a call graph: use callers or callees for a call relationship rooted at a symbol. Go's structural interfaces have no explicit relation edge.",
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
        assert!(build_args("codeq_explore", &json!({})).is_err());
    }

    #[test]
    fn mcp_exposes_precision_controls_and_ast_explanations() {
        let listed = tools();
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
            ("codeq_explore", &["resolved"][..]),
            ("codeq_callers", &["limit"][..]),
            ("codeq_callees", &["limit"][..]),
            ("codeq_astgrep", &["explain"][..]),
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
