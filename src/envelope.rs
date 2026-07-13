//! The `codeq.result.v1` output envelope.
//!
//! Designed for the actual consumer: an AI agent paying tokens per byte. So:
//!
//! - **Compact by default.** Pretty-printing is opt-in (`--meta`/`--pretty`);
//!   the default is single-line JSON.
//! - **Meta only when it carries signal.** Because codeq refreshes on every
//!   read, answers are almost always fresh — so `repo`/`index` blocks are
//!   omitted unless something is noteworthy (files reparsed, staleness, a
//!   partial failure) or the caller asks with `--meta`.
//! - **Projectable.** `--fields` drops columns the agent did not ask for.

use serde::Serialize;
use serde_json::Value;

pub const RESULT_SCHEMA: &str = "codeq.result.v1";

/// Output rendering options, threaded from the CLI.
#[derive(Clone, Copy, Default)]
pub struct Render<'a> {
    /// Emit JSON. Default is compact grep-style text. `meta`/`pretty` imply this.
    pub json: bool,
    /// Force the full `repo`/`index` envelope even when nothing is noteworthy.
    pub meta: bool,
    /// Pretty-print (implies including meta for human reading).
    pub pretty: bool,
    /// If set, keep only these keys on each object in `data`.
    pub fields: Option<&'a [String]>,
}

pub struct Envelope<T: Serialize> {
    repo: RepoMeta,
    index: IndexMeta,
    data: T,
}

impl<T: Serialize> Envelope<T> {
    pub fn new(repo: RepoMeta, index: IndexMeta, data: T) -> Self {
        Envelope { repo, index, data }
    }

    /// Whether the meta blocks carry signal worth the tokens.
    fn noteworthy(&self) -> bool {
        self.index.reindexed_files > 0
            || !self.index.fresh
            || self.index.partial_failures > 0
    }

    pub fn print(&self, r: Render) -> anyhow::Result<()> {
        let mut data = serde_json::to_value(&self.data)?;
        if let Some(fields) = r.fields {
            project(&mut data, fields);
        }

        // JSON is opt-in (`--json`); `--meta`/`--pretty` are JSON concepts so they
        // imply it. The default is compact grep-style text — far fewer tokens.
        if r.json || r.meta || r.pretty {
            let include_meta = r.meta || r.pretty || self.noteworthy();
            let mut out = serde_json::Map::new();
            out.insert("schema".into(), Value::from(RESULT_SCHEMA));
            if include_meta {
                out.insert("repo".into(), serde_json::to_value(&self.repo)?);
                out.insert("index".into(), serde_json::to_value(&self.index)?);
            }
            out.insert("data".into(), data);
            let value = Value::Object(out);
            if r.pretty {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", serde_json::to_string(&value)?);
            }
            return Ok(());
        }

        // Text mode: keep stdout pure data; surface noteworthy meta on stderr so
        // it doesn't pollute a parseable/piped result.
        if self.noteworthy() {
            eprintln!(
                "# codeq: reindexed {} file(s){}{}",
                self.index.reindexed_files,
                if self.index.partial_failures > 0 {
                    format!(", {} partial failure(s)", self.index.partial_failures)
                } else {
                    String::new()
                },
                if self.index.fresh { "" } else { ", index stale" },
            );
        }
        let mut s = String::new();
        render_value(&data, &mut s);
        print!("{s}");
        Ok(())
    }
}

/// Render a `data` payload as compact, deterministic, grep-style text.
///
/// - array of rows → one `path:line:col: payload` line each;
/// - object with a single primary array (`matches`, `references`, …) → its rows;
/// - object with several arrays (`about`, `map`) → each under a `# key` header;
/// - object of scalars (`stats`) → `key: value` lines.
fn render_value(data: &Value, out: &mut String) {
    match data {
        Value::Array(rows) => {
            for row in rows {
                render_row(row, out);
            }
        }
        Value::Object(map) => {
            let arrays = map.values().filter(|v| v.is_array()).count();
            if arrays == 0 {
                for (k, v) in map {
                    out.push_str(&format!("{k}: {}\n", inline(v)));
                }
            } else if arrays == 1 && map.len() <= 2 {
                // A single result list (+ maybe a scalar like `truncated`): just rows.
                for v in map.values() {
                    if let Value::Array(rows) = v {
                        for row in rows {
                            render_row(row, out);
                        }
                    }
                }
            } else {
                for (k, v) in map {
                    match v {
                        Value::Array(rows) => {
                            out.push_str(&format!("# {k}\n"));
                            for row in rows {
                                render_row(row, out);
                            }
                        }
                        _ => out.push_str(&format!("{k}: {}\n", inline(v))),
                    }
                }
            }
        }
        other => {
            out.push_str(&inline(other));
            out.push('\n');
        }
    }
}

/// One result row → `path:line:col: payload`, indented by `depth` for outlines.
fn render_row(row: &Value, out: &mut String) {
    let o = match row {
        Value::Object(m) => m,
        other => {
            out.push_str(&inline(other));
            out.push('\n');
            return;
        }
    };
    let str_of = |k: &str| o.get(k).and_then(|v| v.as_str());
    let int_of = |k: &str| o.get(k).and_then(|v| v.as_i64());

    let mut loc = String::new();
    if let Some(p) = str_of("path") {
        loc.push_str(p);
        if let Some(l) = int_of("line").or_else(|| int_of("start_line")) {
            loc.push_str(&format!(":{l}"));
            if let Some(c) = int_of("col") {
                loc.push_str(&format!(":{c}"));
            }
        }
        loc.push_str(": ");
    } else if let Some(l) = int_of("line").or_else(|| int_of("start_line")) {
        loc.push_str(&format!("{l}: "));
    }

    let pad = "  ".repeat(int_of("depth").unwrap_or(0).max(0) as usize);

    // A multi-line body (context chunks) prints verbatim under a locator header.
    if let Some(t) = str_of("text") {
        if t.contains('\n') {
            out.push_str(&format!("{pad}{loc}\n{t}\n"));
            return;
        }
        // codeq's differentiator over grep: annotate the match with the symbol
        // it sits inside, so an agent needn't a follow-up `at` to place it.
        let enclosing = str_of("symbol")
            .map(|s| format!("  [{s}]"))
            .unwrap_or_default();
        out.push_str(&format!("{pad}{loc}{}{enclosing}\n", t.trim()));
        return;
    }
    if let Some(sig) = str_of("signature") {
        out.push_str(&format!("{pad}{loc}{sig}\n"));
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    for key in ["kind", "name", "module", "symbol"] {
        if let Some(v) = str_of(key) {
            parts.push(v.to_string());
        }
    }
    if parts.is_empty() {
        if let Some(c) = int_of("count") {
            parts.push(c.to_string());
        }
    }
    out.push_str(&format!("{pad}{loc}{}\n", parts.join(" ")));
}

/// Render a scalar (or a small nested object/array) inline on one line.
fn inline(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(m) => m
            .iter()
            .map(|(k, val)| format!("{k}={}", inline(val)))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Array(a) => a.iter().map(inline).collect::<Vec<_>>().join(","),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}

/// Retain only `fields` on each result row, wherever the rows live: a top-level
/// array (`files`, `symbols`, …) or an array nested inside the data object
/// (`matches` for `search`, `references`, `chunks` for `context`, the grouped
/// lists in `map`). Scalar/metadata fields on the wrapper object (e.g.
/// `truncated`) are left intact. Non-object rows are untouched.
fn project(data: &mut Value, fields: &[String]) {
    match data {
        Value::Array(rows) => {
            for row in rows {
                project_obj(row, fields);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                if let Value::Array(rows) = value {
                    for row in rows {
                        project_obj(row, fields);
                    }
                }
            }
        }
        _ => {}
    }
}

fn project_obj(v: &mut Value, fields: &[String]) {
    if let Value::Object(map) = v {
        map.retain(|k, _| fields.iter().any(|f| f == k));
    }
}

#[derive(Serialize)]
pub struct RepoMeta {
    pub root: String,
    pub vcs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

#[derive(Serialize)]
pub struct IndexMeta {
    pub schema: i64,
    pub fresh: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub reindexed_files: u64,
    /// Files that failed to parse/read during this refresh; 0 in the happy path.
    #[serde(skip_serializing_if = "is_zero")]
    pub partial_failures: u64,
    pub elapsed_ms: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}
