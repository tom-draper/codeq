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
    let mut args: Vec<String> = Vec::new();
    match tool {
        "codeq_map" => {
            args.push("map".into());
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_search" => {
            args.push("search".into());
            args.push(s(a, "pattern")?.into());
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
                args.push(p.into());
            }
            opt(&mut args, a, "kind", "--kind");
            opt(&mut args, a, "language", "--language");
            opt(&mut args, a, "path", "--path");
            flag(&mut args, a, "signatures", "--signatures");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_definitions" => {
            args.push("definitions".into());
            args.push(s(a, "symbol")?.into());
            opt(&mut args, a, "path", "--path");
        }
        "codeq_references" => {
            args.push("references".into());
            args.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
            opt(&mut args, a, "language", "--language");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_callers" => {
            args.push("callers".into());
            args.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
        }
        "codeq_callees" => {
            args.push("callees".into());
            args.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
        }
        "codeq_about" => {
            args.push("about".into());
            args.push(s(a, "name")?.into());
            opt(&mut args, a, "path", "--path");
        }
        "codeq_outline" => {
            args.push("outline".into());
            args.push(s(a, "file")?.into());
            flag(&mut args, a, "docs", "--docs");
        }
        "codeq_at" => {
            args.push("at".into());
            args.push(s(a, "location")?.into());
        }
        "codeq_read" => {
            args.push("read".into());
            args.push(s(a, "file")?.into());
            opt(&mut args, a, "lines", "--lines");
            opt(&mut args, a, "symbol", "--symbol");
            flag(&mut args, a, "full", "--full");
        }
        "codeq_imports" => {
            args.push("imports".into());
            if let Some(p) = s(a, "path") {
                args.push(p.into());
            }
        }
        "codeq_importers" => {
            args.push("importers".into());
            args.push(s(a, "query")?.into());
        }
        "codeq_astgrep" => {
            args.push("astgrep".into());
            args.push(s(a, "pattern")?.into());
            args.push("--language".into());
            args.push(s(a, "language")?.into());
            opt(&mut args, a, "path", "--path");
        }
        "codeq_changed" => {
            args.push("changed".into());
            opt(&mut args, a, "compare_to", "--compare-to");
            flag(&mut args, a, "symbols", "--symbols");
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
            flag(&mut args, a, "expand", "--expand");
            flag(&mut args, a, "plan", "--plan");
            optn(&mut args, a, "max_tokens", "--max-tokens");
        }
        "codeq_files" => {
            args.push("files".into());
            if let Some(p) = s(a, "pattern") {
                args.push(p.into());
            }
            opt(&mut args, a, "language", "--language");
            optn(&mut args, a, "limit", "--limit");
        }
        "codeq_stats" => args.push("stats".into()),
        "codeq_capabilities" => args.push("capabilities".into()),
        _ => return None,
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
    let int_ = json!({ "type": "integer" });

    json!([
        tool("codeq_map",
            "Orient in the repo: factual counts by language, directory, imported module, and file. Start here in an unfamiliar codebase.",
            json!({ "limit": int_ }), &[]),
        tool("codeq_search",
            "Search file contents. Regex by default (same engine as ripgrep); set fixed for a literal. Each match is annotated with its enclosing definition. Superset of grep: -A/-B/-C context, files_with_matches, count, and 'in' (code|comments|strings) which grep can't do.",
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
            json!({ "symbol": str_, "path": str_ }), &["symbol"]),
        tool("codeq_references",
            "Word-boundary uses of a symbol name across the repo, each annotated with its enclosing definition; definition sites are flagged. Textual (not type-resolved).",
            json!({ "name": str_, "path": str_, "language": str_, "limit": int_ }), &["name"]),
        tool("codeq_callers",
            "The distinct definitions that reference a symbol (its callers), with test sites flagged — what could break if you change it. Textual, not type-resolved. path restricts to callers located in matching files.",
            json!({ "name": str_, "path": str_ }), &["name"]),
        tool("codeq_callees",
            "The definitions referenced by name inside a symbol's body (its callees) — what it depends on. path scopes to the symbol's definition(s) in matching files (use it when the name is common, e.g. new/build).",
            json!({ "name": str_, "path": str_ }), &["name"]),
        tool("codeq_about",
            "One-call dossier before editing a symbol: its definition(s) with signature and doc, its callers (who breaks), and its callees (what it depends on). Replaces several separate lookups. path scopes the definition and callees to matching files to disambiguate a common name.",
            json!({ "name": str_, "path": str_ }), &["name"]),
        tool("codeq_outline",
            "The structure of a file top to bottom: its definitions with signatures and nesting depth.",
            json!({ "file": str_, "docs": bool_ }), &["file"]),
        tool("codeq_at",
            "Resolve a location (e.g. from a stack trace or compiler error) to the definition(s) enclosing it, innermost first. Location is <path>:<line>.",
            json!({ "location": str_ }), &["location"]),
        tool("codeq_read",
            "Read a file — PREFER THIS over a raw file read for large files. By default it folds long function/method bodies to their signature plus a marker with the exact hidden line range, so you see the whole file's structure cheaply; then expand with lines (verbatim slice, e.g. \"40-80\") or symbol (a named definition, verbatim). full returns the unfolded file. Line-numbered text.",
            json!({ "file": str_, "lines": str_, "symbol": str_, "full": bool_ }), &["file"]),
        tool("codeq_imports",
            "List import statements (raw module strings), optionally scoped to files whose path matches.",
            json!({ "path": str_ }), &[]),
        tool("codeq_importers",
            "Files whose imports match a module string.",
            json!({ "query": str_ }), &["query"]),
        tool("codeq_astgrep",
            "Structural (AST) search: match a code pattern with $NAME / $_ metavariables, parsed with the language's own grammar so it matches syntax not text — e.g. '$X.unwrap()' finds every .unwrap() call whatever the receiver. Deterministic; a repeated $NAME must bind identically. Cannot see inside macro invocations (their bodies aren't parsed into an AST). language is required.",
            json!({ "pattern": str_, "language": str_, "path": str_ }), &["pattern", "language"]),
        tool("codeq_changed",
            "Files changed vs a git ref (default main); set symbols to get the definitions the change touched instead.",
            json!({ "compare_to": str_, "symbols": bool_ }), &[]),
        tool("codeq_context",
            "Assemble token-budgeted code context from explicit seeds: named symbols and/or changed files. Set expand for one deterministic hop to referenced definitions.",
            json!({ "symbols": { "type": "array", "items": str_ }, "changed": bool_, "compare_to": str_, "expand": bool_, "plan": bool_, "max_tokens": int_ }), &[]),
        tool("codeq_files",
            "List indexed files, optionally filtered by path substring or language.",
            json!({ "pattern": str_, "language": str_, "limit": int_ }), &[]),
        tool("codeq_stats",
            "Index summary: file/symbol/import counts by language and total bytes.",
            json!({}), &[]),
        tool("codeq_capabilities",
            "Describe what codeq supports: commands, languages, fact kinds, schema.",
            json!({}), &[]),
    ])
}
