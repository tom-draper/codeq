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

    for line in stdin.lock().lines() {
        let line = line?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => continue, // ignore malformed lines
        };

        // Notifications (no id) get no response.
        let id = match req.get("id") {
            Some(id) if !id.is_null() => id.clone(),
            _ => continue,
        };
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params");

        let result = match method {
            "initialize" => Ok(initialize_result(params)),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(call_tool(params, &exe, &cwd)),
            "ping" => Ok(json!({})),
            other => Err((-32601i64, format!("method not found: {other}"))),
        };

        let msg = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };
        writeln!(out, "{}", serde_json::to_string(&msg)?)?;
        out.flush()?;
    }
    Ok(())
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
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let cli_args = match build_args(name, &args) {
        Some(a) => a,
        None => return tool_error(format!("unknown tool: {name}")),
    };

    // MCP is a programmatic consumer, so request structured JSON (the CLI now
    // defaults to compact text for direct/agent use).
    match Command::new(exe).arg("--json").args(&cli_args).current_dir(cwd).output() {
        Ok(o) if o.status.success() => {
            json!({ "content": [{ "type": "text", "text": String::from_utf8_lossy(&o.stdout) }] })
        }
        Ok(o) => tool_error(format!(
            "codeq error: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => tool_error(format!("failed to run codeq: {e}")),
    }
}

fn tool_error(msg: String) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

// --- argument helpers -------------------------------------------------------

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str())
}
fn b(a: &Value, k: &str) -> bool {
    a.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}
fn n(a: &Value, k: &str) -> Option<i64> {
    a.get(k).and_then(|v| v.as_i64())
}

/// Push `--flag value` if the string arg is present.
fn opt(args: &mut Vec<String>, a: &Value, key: &str, flag: &str) {
    if let Some(v) = s(a, key) {
        args.push(flag.into());
        args.push(v.into());
    }
}
/// Push `--flag N` if the number arg is present.
fn optn(args: &mut Vec<String>, a: &Value, key: &str, flag: &str) {
    if let Some(v) = n(a, key) {
        args.push(flag.into());
        args.push(v.to_string());
    }
}
/// Push `flag` if the bool arg is true.
fn flag(args: &mut Vec<String>, a: &Value, key: &str, flag: &str) {
    if b(a, key) {
        args.push(flag.into());
    }
}

/// Map an MCP tool name + arguments to codeq CLI arguments.
fn build_args(tool: &str, a: &Value) -> Option<Vec<String>> {
    // Flags accumulate in `args`; caller-supplied *values* go to `pos` and are
    // emitted after a `--` separator. Without it a value that happens to start
    // with a dash is read as a flag by the CLI's parser: `pattern: "--json"` —
    // an entirely ordinary thing to search a codebase for — failed as "required
    // argument not provided", and `symbol: "--help"` printed the help page and
    // exited 0, handing an agent the help text as a successful answer.
    let mut args: Vec<String> = Vec::new();
    let mut pos: Vec<String> = Vec::new();
    match tool {
        "codeq_map" => {
            args.push("map".into());
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_search" => {
            args.push("search".into());
            pos.push(s(a, "pattern")?.into());
            flag(&mut args, a, "fixed", "--fixed");
            flag(&mut args, a, "ignore_case", "-i");
            flag(&mut args, a, "word", "-w");
            opt(&mut args, a, "path", "--path");
            opt(&mut args, a, "language", "--language");
            opt(&mut args, a, "in", "--in");
            optn(&mut args, a, "context", "-C");
            flag(&mut args, a, "files_with_matches", "-l");
            flag(&mut args, a, "count", "-c");
            optn(&mut args, a, "limit", "--limit");
            if let Some(globs) = a.get("glob").and_then(|v| v.as_array()) {
                for g in globs.iter().filter_map(|g| g.as_str()) {
                    args.push("-g".into());
                    args.push(g.into());
                }
            }
        }
        "codeq_symbols" => {
            args.push("symbols".into());
            if let Some(p) = s(a, "pattern") {
                pos.push(p.into());
            }
            opt(&mut args, a, "kind", "--kind");
            opt(&mut args, a, "language", "--language");
            opt(&mut args, a, "path", "--path");
            flag(&mut args, a, "signatures", "--signatures");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_definitions" => {
            args.push("definitions".into());
            pos.push(s(a, "symbol")?.into());
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_references" => {
            args.push("references".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            opt(&mut args, a, "language", "--language");
            optn(&mut args, a, "limit", "--limit");
            flag(&mut args, a, "resolved", "--resolved");
            flag(&mut args, a, "calls", "--calls");
            opt(&mut args, a, "on", "--on");
        }
        "codeq_callers" => {
            args.push("callers".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "depth", "--depth");
            flag(&mut args, a, "resolved", "--resolved");
            flag(&mut args, a, "calls", "--calls");
            opt(&mut args, a, "on", "--on");
            flag(&mut args, a, "examples", "--examples");
        }
        "codeq_callees" => {
            args.push("callees".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "depth", "--depth");
            flag(&mut args, a, "resolved", "--resolved");
            flag(&mut args, a, "examples", "--examples");
        }
        "codeq_about" => {
            args.push("about".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
        }
        "codeq_explore" => {
            args.push("explore".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "max_tokens", "--max-tokens");
        }
        // `codeq_path` was the old name; still accepted so a host holding a cached
        // tool list keeps working after the rename.
        "codeq_callpath" | "codeq_path" => {
            args.push("callpath".into());
            pos.push(s(a, "from")?.into());
            pos.push(s(a, "to")?.into());
            optn(&mut args, a, "max_depth", "--max-depth");
            opt(&mut args, a, "path", "--path");
        }
        "codeq_members" => {
            args.push("members".into());
            args.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            opt(&mut args, a, "kind", "--kind");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_untested" => {
            args.push("untested".into());
            if let Some(v) = s(a, "path") {
                args.push(v.into());
            }
            opt(&mut args, a, "kind", "--kind");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_outline" => {
            args.push("outline".into());
            pos.push(s(a, "file")?.into());
            flag(&mut args, a, "docs", "--docs");
        }
        "codeq_at" => {
            args.push("at".into());
            pos.push(s(a, "location")?.into());
        }
        "codeq_read" => {
            args.push("read".into());
            pos.push(s(a, "file")?.into());
            opt(&mut args, a, "lines", "--lines");
            opt(&mut args, a, "symbol", "--symbol");
            flag(&mut args, a, "full", "--full");
            flag(&mut args, a, "raw", "--raw");
        }
        "codeq_imports" => {
            args.push("imports".into());
            if let Some(p) = s(a, "path") {
                pos.push(p.into());
            }
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_importers" => {
            args.push("importers".into());
            pos.push(s(a, "query")?.into());
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_implementors" => {
            args.push("implementors".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "relation", "--relation");
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_supertypes" => {
            args.push("supertypes".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "relation", "--relation");
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_unused" => {
            args.push("unused".into());
            if let Some(p) = s(a, "path") {
                pos.push(p.into());
            }
            opt(&mut args, a, "kind", "--kind");
            flag(&mut args, a, "include_tests", "--include-tests");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_tree" => {
            args.push("tree".into());
            if let Some(p) = s(a, "path") {
                pos.push(p.into());
            }
            optn(&mut args, a, "depth", "--depth");
            optn(&mut args, a, "min_symbols", "--min-symbols");
            flag(&mut args, a, "files", "--files");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_hotspots" => {
            args.push("hotspots".into());
            if let Some(p) = s(a, "path") {
                pos.push(p.into());
            }
            opt(&mut args, a, "kind", "--kind");
            opt(&mut args, a, "language", "--language");
            flag(&mut args, a, "calls", "--calls");
            optn(&mut args, a, "min_refs", "--min-refs");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_api" => {
            args.push("api".into());
            if let Some(p) = s(a, "path") {
                pos.push(p.into());
            }
            opt(&mut args, a, "kind", "--kind");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_deps" => {
            args.push("deps".into());
            if let Some(f) = s(a, "file") {
                args.push(f.into());
            }
            optn(&mut args, a, "depth", "--depth");
            flag(&mut args, a, "cycles", "--cycles");
            flag(&mut args, a, "external", "--external");
        }
        "codeq_astgrep" => {
            args.push("astgrep".into());
            pos.push(s(a, "pattern")?.into());
            // `language` is this flag's value, not a positional — it must stay
            // attached to `--language`, ahead of the `--` separator.
            args.push("--language".into());
            args.push(s(a, "language")?.into());
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_clones" => {
            args.push("clones".into());
            opt(&mut args, a, "path", "--path");
            opt(&mut args, a, "kind", "--kind");
            optn(&mut args, a, "min_nodes", "--min-nodes");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_tests" => {
            args.push("tests".into());
            pos.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_changed" => {
            args.push("changed".into());
            opt(&mut args, a, "compare_to", "--compare-to");
            flag(&mut args, a, "symbols", "--symbols");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_affected" => {
            args.push("affected".into());
            opt(&mut args, a, "compare_to", "--compare-to");
            optn(&mut args, a, "depth", "--depth");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_context" => {
            args.push("context".into());
            // symbols may be an array or a comma string.
            if let Some(arr) = a.get("symbols").and_then(|v| v.as_array()) {
                let joined: Vec<&str> = arr.iter().filter_map(|x| x.as_str()).collect();
                if !joined.is_empty() {
                    args.push("--symbols".into());
                    args.push(joined.join(","));
                }
            } else {
                opt(&mut args, a, "symbols", "--symbols");
            }
            flag(&mut args, a, "changed", "--changed");
            opt(&mut args, a, "compare_to", "--compare-to");
            opt(&mut args, a, "path", "--path");
            flag(&mut args, a, "expand", "--expand");
            flag(&mut args, a, "no_text", "--no-text");
            flag(&mut args, a, "plan", "--plan");
            optn(&mut args, a, "max_tokens", "--max-tokens");
        }
        "codeq_files" => {
            args.push("files".into());
            if let Some(p) = s(a, "pattern") {
                pos.push(p.into());
            }
            opt(&mut args, a, "language", "--language");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_stats" => args.push("stats".into()),
        "codeq_capabilities" => args.push("capabilities".into()),
        _ => return None,
    }
    // `--` ends flag parsing, so every caller-supplied value after it is taken
    // literally however it starts.
    if !pos.is_empty() {
        args.push("--".into());
        args.extend(pos);
    }
    Some(args)
}

/// Tool definitions with JSON Schemas, returned by `tools/list`.
fn tools() -> Value {
    fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
            }
        })
    }
    let str_ = json!({ "type": "string" });
    let bool_ = json!({ "type": "boolean" });
    let num_ = json!({ "type": "integer" });
    let int_ = json!({ "type": "integer" });

    json!([
        tool("codeq_explore",
            "Understand a symbol in ONE call: its definition, who calls it, what it calls, and the verbatim source of that whole neighbourhood, budgeted and grouped by file. Prefer this over a definitions -> callers -> read -> read sequence when you need to understand or change a function/type. name is the exact symbol; path disambiguates a common name; max_tokens caps the source (the definition is always included, callers and callees fill the rest, and anything dropped is listed under omitted). Returns focus, callers, callees, and source (each chunk tagged focus/caller/callee). Same neighbourhood as codeq_about, which omits the source but reports fuller metadata per node — source is the axis the two differ on.",
            json!({ "name": str_, "path": str_, "max_tokens": int_ }), &["name"]),
        tool("codeq_map",
            "Orient in the repo: factual counts by language, directory, imported module, and file. Start here in an unfamiliar codebase.",
            json!({ "limit": int_ }), &[]),
        tool("codeq_search",
            "Search file contents. Regex by default (same engine as ripgrep); set fixed for a literal. Each match is annotated with its enclosing definition. Superset of grep: -A/-B/-C context, files_with_matches, count, and 'in' (code|comments|strings) which grep can't do. In markdown, fenced code blocks count as strings and prose as comments, so 'in' separates documented examples from the text around them.",
            json!({
                "pattern": str_, "fixed": bool_, "ignore_case": bool_, "word": bool_,
                "path": str_, "language": str_, "glob": { "type": "array", "items": str_ },
                "in": { "type": "string", "enum": ["code", "comments", "strings"] },
                "context": int_, "files_with_matches": bool_, "count": bool_, "limit": int_
            }), &["pattern"]),
        tool("codeq_symbols",
            "List defined symbols (functions, classes, structs, interfaces, …), optionally filtered by name substring, kind, language, or path. Set signatures to include each declaration.",
            json!({ "pattern": str_, "kind": str_, "language": str_, "path": str_, "signatures": bool_, "limit": int_ }), &[]),
        tool("codeq_definitions",
            "Locate the definition(s) of an exact symbol name, with signature, doc, and precise byte/line ranges. path scopes to matching files.",
            json!({ "symbol": str_, "path": str_, "limit": int_ }), &["symbol"]),
        tool("codeq_references",
            "Word-boundary uses of a symbol name across the repo, each annotated with its enclosing definition; definition sites are flagged. Textual (not type-resolved) by default. resolved=true prunes references whose file can't reach a definition through imports. calls=true keeps only real call sites and on='RECV' keeps only calls on that receiver; both are read from the parse tree, so a declaration, a mention in a string or comment, and formatting (a wrapped method chain) are all handled correctly. The receiver is the expression as written, so 'db' does not match mydb.query() or a.inner.query() — use on='a.inner', or even on='conn()'; PHP receivers keep their sigil (on='$db'). Precision filters compose.",
            json!({ "name": str_, "path": str_, "language": str_, "limit": int_, "resolved": bool_, "calls": bool_, "on": str_ }), &["name"]),
        tool("codeq_callers",
            "The distinct definitions that reference a symbol (its callers), with test sites flagged — what could break if you change it. Textual, not type-resolved. path restricts to callers in matching files. depth (default 1) walks transitively up the call tree, each result tagged with its hop distance. Precision filters (direct callers only): resolved=true prunes callers whose file can't reach a definition through imports; calls=true keeps only real call sites; on='RECV' keeps only calls on that receiver. Both come from the parse tree, so declarations, prose mentions and line breaks are handled; the receiver is the expression as written (on='a.inner', on='conn()', PHP on='$db'). examples=true surfaces the actual call expression at each site (one representative per caller) — how the symbol is really invoked; for every call site use codeq_references with calls=true.",
            json!({ "name": str_, "path": str_, "depth": num_, "resolved": bool_, "calls": bool_, "on": str_, "examples": bool_ }), &["name"]),
        tool("codeq_callees",
            "The definitions referenced by name inside a symbol's body (its callees) — what it depends on. Already call-position only: a bare mention isn't a callee, and neither is helper() written inside a string or a comment. path scopes to the symbol's definition(s). depth (default 1) walks transitively down the call tree. resolved=true (direct callees only) keeps only callees whose definition file the symbol's own file can reach through imports, pruning unrelated same-named definitions in unreachable files. examples=true adds `call` — the call as written, with its receiver (`subprocess.run`, not `run`) — and `call_line`; use it to see at a glance whether a row is a real callee or a name collision, since a module-qualified call is attributed to every definition sharing the bare name.",
            json!({ "name": str_, "path": str_, "depth": num_, "resolved": bool_, "examples": bool_ }), &["name"]),
        tool("codeq_about",
            "One-call dossier before editing a symbol, WITHOUT source: its definition(s) with signature and doc comment, its callers (who breaks, with test call sites flagged), and its callees (what it depends on). Replaces several separate lookups. path scopes the definition and callees to matching files to disambiguate a common name. Same neighbourhood as codeq_explore, which adds the verbatim source but trims the per-node metadata — reach for codeq_explore when you intend to change the code, and this when you want the metadata or a cheap 'where does this live'.",
            json!({ "name": str_, "path": str_ }), &["name"]),
        tool("codeq_callpath",
            "The shortest CALL chain from one symbol to another (from calls … calls to), if one exists — reachability with the route shown, e.g. how an entry point reaches a low-level function. Nothing to do with file paths. Name-based; empty when unreachable within max_depth (default 8). path scopes the start symbol to matching files, to disambiguate a common/cross-language `from` like `main`.",
            json!({ "from": str_, "to": str_, "max_depth": num_, "path": str_ }), &["from", "to"]),
        tool("codeq_untested",
            "Exported definitions that no test refers to — api composed with tests, which otherwise costs one call per public symbol. Public only (a private helper is usually covered through the exported thing that calls it). Heuristic in the same way tests is, so it over-reports coverage rather than under-report it: a test that reaches the code without naming it is invisible.",
            json!({ "path": str_, "kind": str_, "limit": int_ }), &[]),
        tool("codeq_members",
            "What a type contains: its fields, enum variants, and methods, in source order. Use it to learn a struct/class/enum's shape before constructing or destructuring it, instead of reading the file. Unlike the call-graph commands this is structural, not name-based: a member is a definition nested directly inside the type's byte range, so there are no same-name false positives. kind filters to one of field/variant/method/function; path disambiguates a common type name. Languages that don't declare members (Ruby attr_accessor, Python self.x in __init__) return nothing rather than a guess, and in Rust a struct's methods live in a separate impl block so only its fields are returned.",
            json!({ "name": str_, "path": str_, "kind": str_, "limit": int_ }), &["name"]),
        tool("codeq_outline",
            "The structure of a file top to bottom: its definitions with signatures and nesting depth. Works on markdown too, where a heading is a definition — so this is a document's table of contents. docs=true adds each definition's doc comment (or a section's first line).",
            json!({ "file": str_, "docs": bool_ }), &["file"]),
        tool("codeq_at",
            "Resolve a location (e.g. from a stack trace or compiler error) to the definition(s) enclosing it, innermost first. Location is <path>:<line>.",
            json!({ "location": str_ }), &["location"]),
        tool("codeq_read",
            "Read a file — PREFER THIS over a raw file read for large files. By default it folds long function/method bodies to their signature plus a marker with the exact hidden line range, so you see the whole file's structure cheaply; then expand with lines (verbatim slice, e.g. \"40-80\") or symbol (a named definition, verbatim). full returns the unfolded file. Line-numbered text. raw=true drops the line-number prefixes and folding to emit exact bytes — use it to grab a symbol's source as an edit's search text (old_string); with symbol it requires a single match. On markdown, folding collapses each section's prose and leaves the heading tree, and symbol reads one section by its heading — prefer that over reading a whole README.",
            json!({ "file": str_, "lines": str_, "symbol": str_, "full": bool_, "raw": bool_ }), &["file"]),
        tool("codeq_imports",
            "List import statements (raw module strings), optionally scoped to files whose path matches.",
            json!({ "path": str_, "limit": int_ }), &[]),
        tool("codeq_importers",
            "Files whose imports match a module string.",
            json!({ "query": str_, "limit": int_ }), &["query"]),
        tool("codeq_implementors",
            "Types that implement or extend a trait / interface / base class — the inheritance edges the call graph doesn't cover (impl T for X in Rust, extends/implements in Java/TS/PHP, class X(Base) in Python, class X < Y in Ruby, base lists in C++/C#). Use it to answer 'who implements this interface' before changing it. Go (structural typing) and C have none. Name-based. relation filters to 'implements' or 'extends'.",
            json!({ "name": str_, "relation": str_, "path": str_, "limit": num_ }), &["name"]),
        tool("codeq_supertypes",
            "The traits / interfaces / base classes a type extends or implements — the reverse of implementors (what does X derive from). The names as written at the declaration, not transitively resolved. Name-based. relation filters to 'implements' or 'extends'.",
            json!({ "name": str_, "relation": str_, "path": str_, "limit": num_ }), &["name"]),
        tool("codeq_unused",
            "Definitions with no reference anywhere outside their own definition site — candidate dead code. A mention in a comment is not a use, so a stale TODO cannot mask dead code; a mention in a string still is, because reflection, serde renames, config keys and SQL identifiers are real uses. Name-based, so a same-named symbol elsewhere masks it; it under-reports rather than misleads. visibility is shown so a public symbol (maybe external API, not dead) is distinguishable from a private one. Test-file definitions are excluded by default (framework entry points look unused); set include_tests to include them. path/kind scope what is reported; the usage check is always repo-wide.",
            json!({ "path": str_, "kind": str_, "include_tests": bool_, "limit": num_ }), &[]),
        tool("codeq_tree",
            "The repo's directory SHAPE, folded to a depth, with each directory's file and symbol counts aggregated from everything below it. Use this to orient in an unfamiliar repo: unlike tree(1) it is ignore-aware (no node_modules/.git noise) and reports where the *code* is rather than just where files are; unlike codeq_map it keeps the nesting instead of collapsing to top-level directories. min_symbols=1 jumps straight to the directories that hold code, which is the fastest way to find the source in a data- or asset-heavy repo. depth defaults to 3; counts still include everything below the fold, so folding summarises rather than truncates.",
            json!({ "path": str_, "depth": num_, "min_symbols": num_, "files": bool_, "limit": num_ }), &[]),
        tool("codeq_hotspots",
            "Definitions ranked by how often their name is referenced in code — the inverse of codeq_unused, and the fastest way to find what is load-bearing in a file or repo you have just met. One row per name (the count is a property of the name), with defs telling you how many definitions share it, and path/line pointing at the first. Set calls=true to count only call sites, which is what you usually want for functions: without it a name that is also a common variable scores high. Name-based, so a short common name (new, get, path) ranks high because many unrelated symbols spell it the same way — treat a row as 'this identifier carries weight' and use codeq_callers with calls=true for the precise question. path/kind/language scope what is reported; the count is always repo-wide.",
            json!({ "path": str_, "kind": str_, "language": str_, "calls": bool_, "min_refs": num_, "limit": num_ }), &[]),
        tool("codeq_api",
            "The public API surface of a file or directory. compare_to=<git ref> instead reports how that surface differs from the ref — what was added, removed, or changed shape — which is the review question a raw diff cannot answer, since most edited lines are internal. Otherwise: the definitions it exports for outside use, with signatures. \"Public\" is each language's own notion (Rust pub, JS/TS export, Go capitalization, Python non-underscore, a public modifier in Java/C#/PHP, non-static in C/C++); pub(crate)/protected are excluded. Use it to learn how to call an unfamiliar module without reading whole files. path scopes to a file/dir substring; kind filters to one kind (function, struct, class, …).",
            json!({ "path": str_, "kind": str_, "limit": num_ }), &[]),
        tool("codeq_deps",
            "Internal files a file depends on, resolved from its imports (raw import strings matched to file stems — heuristic, not a real module resolver). Pass file, cycles=true, or external=true — calling with none of them is an error. depth (default 1) walks transitive dependencies. cycles=true reports every import cycle in the repo instead (omit file). external=true instead lists imports that resolve to no file in the repo: the third-party dependency list, plus broken local references (a markdown link or ./path pointing at nothing) flagged broken=true.",
            json!({ "file": str_, "depth": num_, "cycles": bool_, "external": bool_ }), &[]),
        tool("codeq_astgrep",
            "Structural (AST) search: match a code pattern with $NAME / $_ metavariables (and $$$ for any run of siblings, e.g. foo($$$)), parsed with the language's own grammar so it matches syntax not text — e.g. '$X.unwrap()' finds every .unwrap() call whatever the receiver. Deterministic; a repeated $NAME must bind identically. Cannot see inside macro invocations. language is required.",
            json!({ "pattern": str_, "language": str_, "path": str_, "limit": int_ }), &["pattern", "language"]),
        tool("codeq_clones",
            "Definitions whose syntax shape is identical up to renaming — the same code written twice. Exact, not fuzzy: shapes either match or they don't, so there is no similarity score to interpret and no threshold to tune. Rows are grouped by a shared shape hash, biggest duplication first. Use it before writing a helper (is this already implemented?) and to find the copy-paste that needs the same fix in several places. min_nodes (default 24) sets how much code a shared shape must cover; kind narrows to e.g. function.",
            json!({ "path": str_, "kind": str_, "min_nodes": int_, "limit": int_ }), &[]),
        tool("codeq_tests",
            "Which tests exercise a symbol: the test functions that reference the given name (heuristic — a test-file path or test-like function name). Great for the edit-then-find-the-test loop.",
            json!({ "name": str_, "path": str_, "limit": int_ }), &["name"]),
        tool("codeq_changed",
            "Files changed vs a git ref (default main); set symbols to get the definitions the change touched instead.",
            json!({ "compare_to": str_, "symbols": bool_, "limit": int_ }), &[]),
        tool("codeq_affected",
            "For a diff (vs compare_to, default main): the definitions it touched, their transitive callers up to depth hops (the impacted set), and the tests that exercise any of them — 'what should I re-check and re-run for this change'. Returns changed, impacted, and tests. Name-based like the rest of the graph.",
            json!({ "compare_to": str_, "depth": num_, "limit": int_ }), &[]),
        tool("codeq_context",
            "Assemble token-budgeted code context from explicit seeds: named symbols and/or changed files. path scopes the seeds to matching files. Set expand for one deterministic hop to referenced definitions; no_text returns provenance only (locations without source, for a cheap map).",
            json!({ "symbols": { "type": "array", "items": str_ }, "changed": bool_, "compare_to": str_, "path": str_, "expand": bool_, "no_text": bool_, "plan": bool_, "max_tokens": int_ }), &[]),
        tool("codeq_files",
            "List indexed files, optionally filtered by path substring or language.",
            json!({ "pattern": str_, "language": str_, "limit": int_ }), &[]),
        tool("codeq_stats",
            "Index summary: file/symbol/import counts by language and total bytes.",
            json!({}), &[]),
        tool("codeq_capabilities",
            "Describe what codeq supports: commands, languages, fact kinds, schema, and language_support — a per-language matrix of which extractions exist (definitions, imports, members, relations, spans, calls). Check it before reading an empty result as evidence of absence: Go and C have no inheritance edges at all, Ruby declares no fields, and several languages have no import syntax codeq reads.",
            json!({}), &[]),
    ])
}
